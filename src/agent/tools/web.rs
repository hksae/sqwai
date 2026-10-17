//! Local Web Research Engine: multi-source meta-search (RRF + BM25),
//! smart domain adapters, LLM-native Markdown negotiation (`llms.txt` v2),
//! DOM Readability HTML-to-Markdown conversion, and section-cached BM25 navigation.

pub mod markdown;
pub mod meta_search;
pub mod ranker;
pub mod router;

use super::Outcome;
use reqwest::{Client, Url};
use serde_json::Value;
use std::time::Duration;

/// Output cap in chars: head+tail mid-trim, same budget as exec/git.
const MAX_OUTPUT_CHARS: usize = 30_000;

fn url_arg(args: &Value, allow_hosts: &[String]) -> Result<Url, String> {
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
    // shared SSRF gate (see crate::config): initial URL and redirects
    host_allowed(&url, allow_hosts)?;
    Ok(url)
}

/// Thin webfetch spelling over the shared gate: same rules, our prefix.
/// `[web].allow_hosts` is the user's opt-in for loopback targets.
fn host_allowed(url: &Url, allow_hosts: &[String]) -> Result<(), String> {
    crate::config::url_host_allowed(url, allow_hosts).map_err(|detail| format!("webfetch {detail}"))
}

pub(crate) fn client(
    timeout: u64,
    agent: &'static str,
    allow_hosts: Vec<String>,
) -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_secs(timeout.clamp(1, 60)))
        .redirect(redirect_policy(allow_hosts))
        .user_agent(agent)
        .build()
        .map_err(|e| format!("client error: {e}"))
}

/// Redirects re-enter the SSRF gate at every hop with the same 5-hop
/// budget as before: a benign short link follows, a bounce into
/// 127.0.0.1 or metadata stops the request. The allowlist rides along, so a
/// redirect off a listed dev host onto an unlisted one still stops.
fn redirect_policy(allow_hosts: Vec<String>) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt: reqwest::redirect::Attempt| {
        if attempt.previous().len() >= 5 {
            return attempt.stop();
        }
        match host_allowed(attempt.url(), &allow_hosts) {
            Ok(()) => attempt.follow(),
            Err(_) => attempt.stop(),
        }
    })
}

pub async fn fetch(args: &Value, allow_hosts: &[String]) -> Outcome {
    let url = match url_arg(args, allow_hosts) {
        Ok(url) => url,
        Err(error) => return Outcome::err(error),
    };
    let selector = args
        .get("selector")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let section = args
        .get("section")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let force_refresh = args
        .get("refresh")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Fast path: if no raw CSS selector is requested and the page is already in
    // our session LRU cache (from an earlier webfetch or deep websearch), serve
    // the requested section/query projection immediately in 0ms.
    if selector.is_none()
        && !force_refresh
        && let Some(cached) = ranker::get_cached_page(url.as_str())
    {
        return Outcome::ok(ranker::project_page(&cached, query, section));
    }

    let timeout = args.get("timeout").and_then(Value::as_u64).unwrap_or(15);
    let client = match client(timeout, crate::providers::USER_AGENT, allow_hosts.to_vec()) {
        Ok(c) => c,
        Err(e) => return Outcome::err(format!("webfetch {e}")),
    };

    let doc = match router::fetch_smart(&client, &url, allow_hosts, selector.is_some()).await {
        Ok(d) => d,
        Err(e) => return Outcome::err(e),
    };

    if let Some(sel) = selector {
        let Some(raw_html) = doc.raw_html.as_deref() else {
            return Outcome::err(
                "webfetch selector requires an HTML page; drop the selector for plain text",
            );
        };
        match select_fragments(raw_html, sel) {
            Ok(fragments) => return Outcome::ok(truncate(&fragments)),
            Err(e) => return Outcome::err(format!("webfetch: {e}")),
        }
    }

    let cached = ranker::put_cached_page(url.as_str(), doc.title, doc.markdown);
    Outcome::ok(ranker::project_page(&cached, query, section))
}

/// Extract the structured Markdown/text of every element matching a CSS selector,
/// one block per element. Errors on a malformed selector or when nothing matches.
fn select_fragments(html: &str, selector: &str) -> Result<String, String> {
    markdown::extract_selector_markdown(html, selector, None)
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
        .unwrap_or(6)
        .clamp(1, 10) as usize;
    let timeout = args.get("timeout").and_then(Value::as_u64).unwrap_or(15);
    let category = meta_search::SearchCategory::parse(args.get("category").and_then(Value::as_str));
    let deep = args.get("deep").and_then(Value::as_bool).unwrap_or(true);

    match meta_search::run_meta_search(query, count, timeout, category, deep, &[]).await {
        Ok(output) => Outcome::ok(truncate(&output)),
        Err(err) => Outcome::err(format!("websearch {err}")),
    }
}

pub(crate) fn looks_like_html(raw: &str) -> bool {
    let head = raw
        .trim_start()
        .get(..512)
        .unwrap_or(raw.trim_start())
        .to_ascii_lowercase();
    head.starts_with("<!doctype html")
        || head.starts_with("<html")
        || head.contains("<body")
        || head.contains("<head")
        || head.contains("<div")
}

pub(crate) fn unwrap_duckduckgo_url(raw: &str) -> String {
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

pub(crate) fn clean_fragment(html: &str) -> String {
    let tags = regex::Regex::new(r"(?s)<[^>]+>").unwrap();
    decode_entities(&tags.replace_all(html, " "))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
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

#[cfg(test)]
fn parse_search_results(html: &str, count: usize) -> String {
    let hits = meta_search::parse_ddg_hits(html, count);
    hits.iter()
        .enumerate()
        .map(|(i, h)| format!("{}. {}\n   {}\n   {}", i + 1, h.title, h.url, h.snippet))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn decode_entities(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
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
            url_arg(&json!({"url": " https://example.com/a "}), &[])
                .unwrap()
                .path(),
            "/a"
        );
        for value in ["", "not a url", "file:///tmp/a", "https://"] {
            assert!(
                url_arg(&json!({"url": value}), &[]).is_err(),
                "accepted {value:?}"
            );
        }
    }

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
                url_arg(&json!({"url": value}), &[]).is_err(),
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
                url_arg(&json!({"url": value}), &[]).is_ok(),
                "SSRF gate blocked {value:?}"
            );
        }
        let bounced: Url = "http://127.0.0.1:8080/".parse().unwrap();
        assert!(host_allowed(&bounced, &[]).is_err());
        let fine: Url = "https://example.com/target".parse().unwrap();
        assert!(host_allowed(&fine, &[]).is_ok());
    }

    #[test]
    fn a_listed_loopback_host_passes_the_gate() {
        let allow: Vec<String> = ["localhost:3000", "127.0.0.1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        for value in [
            "http://localhost:3000/api",
            "http://LOCALHOST:3000/api",
            "http://127.0.0.1:11434/v1",
        ] {
            assert!(
                url_arg(&json!({"url": value}), &allow).is_ok(),
                "listed loopback must pass: {value}"
            );
        }
        for value in [
            "http://localhost:3001/",
            "http://api.localhost/",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.1/",
        ] {
            assert!(
                url_arg(&json!({"url": value}), &allow).is_err(),
                "must stay refused with the list present: {value}"
            );
        }
        let hop: Url = "http://localhost:3000/next".parse().unwrap();
        assert!(host_allowed(&hop, &allow).is_ok());
        let offlist: Url = "http://localhost:3001/next".parse().unwrap();
        assert!(host_allowed(&offlist, &allow).is_err());
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
        assert!(out.contains("\n\n"), "{}", out);

        let out = select_fragments(html, "tr td:first-child").unwrap();
        assert!(out.contains("alpha") && out.contains("beta"), "{}", out);
        assert!(!out.contains("\n1"), "{}", out);

        assert!(select_fragments(html, "td[").is_err());
        assert!(select_fragments(html, ".does-not-exist").is_err());
    }

    #[test]
    fn html_to_markdown_preserves_code_blocks_headings_tables_and_strips_nav() {
        let html = r#"
        <!doctype html>
        <html>
        <head>
            <title>Tokio Spawn Guide</title>
            <link rel="alternate" type="text/markdown" href="/docs/spawn.md" />
        </head>
        <body>
            <nav><a href="/home">Home</a><a href="/pricing">Pricing</a></nav>
            <div class="cookie-banner">Accept all cookies</div>
            <main>
                <h1>Spawning Tasks ¶</h1>
                <p>Use <code>tokio::spawn</code> to run an async task concurrently. See <a href="/api/task">Task API</a>.</p>
                <pre><code class="language-rust">#[tokio::main]
async fn main() {
    let handle = tokio::spawn(async {
        // indented comment inside code
        println!("hello");
    });
    handle.await.unwrap();
}</code></pre>
                <h2>Comparison</h2>
                <table>
                    <tr><th>Function</th><th>Pool</th></tr>
                    <tr><td>spawn</td><td>Async worker</td></tr>
                    <tr><td>spawn_blocking</td><td>Blocking pool</td></tr>
                </table>
            </main>
            <footer>Copyright 2026</footer>
        </body>
        </html>
        "#;
        let base = Url::parse("https://tokio.rs/tokio/tutorial/spawning").unwrap();
        let doc = markdown::html_to_markdown(html, Some(&base));

        assert_eq!(
            doc.alternate_markdown_url.as_deref(),
            Some("https://tokio.rs/docs/spawn.md")
        );
        assert!(!doc.markdown.contains("Pricing"), "nav must be stripped");
        assert!(
            !doc.markdown.contains("Accept all cookies"),
            "cookie banner must be stripped"
        );
        assert!(
            !doc.markdown.contains("Copyright 2026"),
            "footer must be stripped"
        );
        assert!(doc.markdown.contains("# Spawning Tasks"));
        assert!(doc.markdown.contains("`tokio::spawn`"));
        assert!(doc.markdown.contains("[Task API](https://tokio.rs/api/task)"));
        assert!(doc.markdown.contains("```rust\n#[tokio::main]\nasync fn main() {\n    let handle = tokio::spawn(async {\n        // indented comment inside code"));
        assert!(doc.markdown.contains("| Function | Pool |"));
        assert!(doc.markdown.contains("| spawn_blocking | Blocking pool |"));
    }

    #[test]
    fn ranker_splits_sections_and_ranks_code_and_identifiers() {
        let md = r#"# Crate Overview

Introductory paragraph about the HTTP client.

## Installation

Add `reqwest` to Cargo.toml.

## Authentication & Bearer Headers

To send a bearer token or custom authorization header, use `RequestBuilder::bearer_auth`:

```rust
let res = client
    .get("https://api.example.com/data")
    .bearer_auth("secret_token")
    .send()
    .await?;
```

## Timeouts

Configure connection and request timeouts with `Duration::from_secs`.
"#;
        let sections = ranker::parse_sections(md);
        assert_eq!(sections.len(), 4);
        // Ensure `#` inside code blocks never creates fake sections.
        let ranked = ranker::rank_sections(&sections, "bearer_auth header");
        assert!(!ranked.is_empty());
        assert_eq!(sections[ranked[0].0].heading, "Authentication & Bearer Headers");

        let page = ranker::put_cached_page(
            "https://example.com/docs/reqwest?utm_source=test#top",
            Some("Reqwest Docs".into()),
            md.into(),
        );
        // Canonical URL lookup hits cache even without tracking params.
        let cached = ranker::get_cached_page("https://www.example.com/docs/reqwest/").unwrap();
        assert_eq!(cached.canonical_url, "https://example.com/docs/reqwest");

        let sec_proj = ranker::project_page(&page, None, Some("Timeouts"));
        assert!(sec_proj.contains("Duration::from_secs"));
        assert!(!sec_proj.contains("Cargo.toml"));
    }

    #[test]
    fn router_classifies_developer_urls_accurately() {
        let gh_blob = Url::parse("https://github.com/tokio-rs/tokio/blob/master/tokio/src/lib.rs").unwrap();
        assert_eq!(
            router::classify_url(&gh_blob),
            router::DomainRoute::GitHubBlob {
                owner: "tokio-rs".into(),
                repo: "tokio".into(),
                rest: "master/tokio/src/lib.rs".into(),
            }
        );

        let gh_issue = Url::parse("https://github.com/ratatui/ratatui/issues/1024").unwrap();
        assert_eq!(
            router::classify_url(&gh_issue),
            router::DomainRoute::GitHubIssueOrPr {
                owner: "ratatui".into(),
                repo: "ratatui".into(),
                kind: "Issue",
                number: 1024,
            }
        );

        let crate_url = Url::parse("https://crates.io/crates/serde/1.0.210").unwrap();
        assert_eq!(
            router::classify_url(&crate_url),
            router::DomainRoute::CratesIo {
                crate_name: "serde".into(),
            }
        );

        let so_url = Url::parse("https://stackoverflow.com/questions/123456/how-to-spawn-in-tokio").unwrap();
        assert_eq!(
            router::classify_url(&so_url),
            router::DomainRoute::StackExchangeQuestion {
                site: "stackoverflow".into(),
                question_id: 123456,
            }
        );
    }

    #[test]
    fn meta_search_parsers_and_rrf_fusion_merge_duplicates() {
        let sp_html = r#"
            <div class="result">
                <a class="result-title result-link" href="https://tokio.rs/tokio/tutorial/spawning">
                    <h2 class="wgl-title">Spawning | Tokio</h2>
                </a>
                <p class="description">Tasks are created with tokio::spawn.</p>
            </div>
        "#;
        let sp_hits = meta_search::parse_startpage_results(sp_html, 5);
        assert_eq!(sp_hits.len(), 1);
        assert_eq!(sp_hits[0].url, "https://tokio.rs/tokio/tutorial/spawning");

        let bing_html = r#"
            <li class="b_algo">
                <h2><a href="https://www.bing.com/ck/a?!&amp;&amp;p=123&amp;u=a1aHR0cHM6Ly90b2tpby5ycy90b2tpby90dXRvcmlhbC9zcGF3bmluZw&amp;ntb=1">Spawning | Tokio Runtime</a></h2>
                <div class="b_caption"><p>Spawning submits the task to the Tokio scheduler.</p></div>
            </li>
        "#;
        let bing_hits = meta_search::parse_bing_results(bing_html, 5);
        assert_eq!(bing_hits.len(), 1);
        assert_eq!(bing_hits[0].url, "https://tokio.rs/tokio/tutorial/spawning");

        let merged = meta_search::fuse_and_rank(
            vec![sp_hits, bing_hits],
            "tokio spawn",
            meta_search::SearchCategory::Auto,
            5,
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].sources, vec!["google", "bing"]);
    }

    #[test]
    fn ddg_captcha_is_detected_and_rejected() {
        let captcha = r#"<html><body><form id="challenge-form">Select all squares containing a duck</form></body></html>"#;
        assert!(meta_search::is_ddg_captcha(captcha));
    }

    #[test]
    fn code_table_and_definition_list_convert_cleanly() {
        let html = r#"
        <main>
            <h2>API Items</h2>
            <dl>
                <dt><code>pub fn spawn&lt;F&gt;(future: F) -&gt; JoinHandle&lt;F::Output&gt;</code></dt>
                <dd>Spawns a new asynchronous task, returning a <code>JoinHandle</code> for it.</dd>
            </dl>
            <table class="highlight">
                <tr>
                    <td class="lineno"><pre>1
2</pre></td>
                    <td class="code"><pre class="rust">let x = 1;
let y = 2;</pre></td>
                </tr>
            </table>
        </main>
        "#;
        let doc = markdown::html_to_markdown(html, None);
        assert!(doc.markdown.contains("**`pub fn spawn<F>(future: F) -> JoinHandle<F::Output>`**"));
        assert!(doc.markdown.contains(": Spawns a new asynchronous task"));
        assert!(doc.markdown.contains("```rust\nlet x = 1;\nlet y = 2;\n```"));
        assert!(!doc.markdown.contains("| 1"), "line-number table should render as fenced code, not pipe table");
    }

    #[test]
    fn pre_nested_spans_table_multiline_cells_and_slashed_branches() {
        // 1. Slashed branch names in GitHub blob URLs map 1:1.
        let slashed = Url::parse("https://github.com/org/vm/blob/feature/gc-opt/src/vm.rs").unwrap();
        assert_eq!(
            router::classify_url(&slashed),
            router::DomainRoute::GitHubBlob {
                owner: "org".into(),
                repo: "vm".into(),
                rest: "feature/gc-opt/src/vm.rs".into(),
            }
        );

        // 2. Nested <span> inside <pre><code> must not insert spaces between tokens,
        //    and line-number gutters (.src-line-numbers) must be stripped.
        // 3. Multi-line <p> and <br> inside <td> must convert to <br> on a single table row.
        let html = r#"
        <main>
            <pre class="rust"><span class="src-line-numbers">1
2</span><code><span>pub</span> <span>fn</span> <span>main</span><span>()</span> <span>{</span>
    <span>ImmixAllocator</span><span>::</span><span>alloc_frame</span><span>();</span>
<span>}</span></code></pre>
            <table>
                <tr><th>Method</th><th>Details</th></tr>
                <tr>
                    <td><code>alloc_frame</code></td>
                    <td><p>First line of cell.</p><p>Second line with <code>a | b</code> pipe.</p></td>
                </tr>
            </table>
        </main>
        "#;
        let doc = markdown::html_to_markdown(html, None);
        assert!(
            doc.markdown.contains("pub fn main() {\n    ImmixAllocator::alloc_frame();\n}"),
            "nested spans must not add spaces and gutter numbers must be stripped:\n{}",
            doc.markdown
        );
        assert!(
            doc.markdown.contains("| `alloc_frame` | First line of cell. <br> Second line with `a \\| b` pipe. |"),
            "table cell newlines must become <br> and pipes must be escaped:\n{}",
            doc.markdown
        );
    }

    #[test]
    fn identifier_tokenizer_breadcrumbs_url_canonicalization_and_global_deep_pool() {
        // 1. Multi-level identifier tokenization (`ImmixAllocator::alloc_frame()`).
        let tokens = ranker::tokenize_for_bm25("ImmixAllocator::alloc_frame() where T: From<U>");
        assert!(tokens.contains(&"immixallocator::alloc_frame".to_string()));
        assert!(tokens.contains(&"immixallocator".to_string()));
        assert!(tokens.contains(&"alloc_frame".to_string()));
        assert!(tokens.contains(&"immix".to_string()));
        assert!(tokens.contains(&"alloc".to_string()));
        assert!(tokens.contains(&"frame".to_string()));
        // Programming keywords `where` and `from` are preserved!
        assert!(tokens.contains(&"where".to_string()));
        assert!(tokens.contains(&"from".to_string()));

        // 2. Deep URL canonicalization (old.reddit.com, www., .git, #fragment).
        assert_eq!(
            ranker::canonicalize_url("https://old.reddit.com/r/rust/comments/abc/title/?utm_source=share#t1"),
            "https://reddit.com/r/rust/comments/abc/title"
        );
        assert_eq!(
            ranker::canonicalize_url("https://www.github.com/ratatui/ratatui.git/"),
            "https://github.com/ratatui/ratatui"
        );

        // 3. Package-to-GitHub-repo cross-linking in RRF.
        let gh_hit = meta_search::RawSearchHit {
            title: "ratatui/ratatui".into(),
            url: "https://github.com/ratatui/ratatui".into(),
            snippet: "TUI library".into(),
            source: "google",
            badge: Some("★ 22996".into()),
            boost: 0.0,
            repo_url: None,
        };
        let crate_hit = meta_search::RawSearchHit {
            title: "ratatui (Rust crate v0.30.2)".into(),
            url: "https://crates.io/crates/ratatui".into(),
            snippet: "TUI library".into(),
            source: "crates.io",
            badge: Some("v0.30.2".into()),
            boost: 0.0,
            repo_url: Some("https://github.com/ratatui/ratatui.git".into()),
        };
        let fused = meta_search::fuse_and_rank(
            vec![vec![gh_hit], vec![crate_hit]],
            "ratatui",
            meta_search::SearchCategory::Auto,
            5,
        );
        let gh_merged = fused.iter().find(|h| h.url.contains("github.com")).unwrap();
        assert!(gh_merged.sources.contains(&"google") && gh_merged.sources.contains(&"crates.io"));
        assert!(gh_merged.badges.contains(&"v0.30.2".to_string()));

        // 4. Breadcrumbs (`<!-- Section: # Struct Tokio / ## fn spawn / ### Panics -->`)
        //    and Global Cross-Page BM25 Pool lifting Page 3's exact code match over Page 1's generic prose.
        let page1 = ranker::put_cached_page(
            "https://example.com/blog/intro",
            Some("General Intro".into()),
            "# General Intro\n\nThis blog talks generally about memory management in many languages.".into(),
        );
        let page3 = ranker::put_cached_page(
            "https://example.com/docs/vm",
            Some("VM Internals".into()),
            "# Struct Immix\n\nOverview.\n\n## fn alloc_frame\n\nAllocates.\n\n### Safety & Panics\n\n```rust\nImmixAllocator::alloc_frame(size);\n```".into(),
        );
        let u1 = Url::parse("https://example.com/blog/intro").unwrap();
        let u3 = Url::parse("https://example.com/docs/vm").unwrap();
        let global = ranker::extract_global_deep_excerpts(
            &[(u1, page1, 0), (u3, page3, 2)],
            "ImmixAllocator alloc_frame",
            800,
        );
        assert!(
            global.contains("<!-- Section: # Struct Immix / ## fn alloc_frame / ### Safety & Panics -->"),
            "missing ancestor breadcrumb comment in:\n{global}"
        );
        assert!(
            global.contains("ImmixAllocator::alloc_frame(size);"),
            "global BM25 pool must pick Page 3's exact code match:\n{global}"
        );
    }

    #[tokio::test]
    #[ignore = "hits live internet endpoints; run with --ignored"]
    async fn live_meta_search_and_cached_section_fetch_smoke() {
        let search_out = search(&json!({
            "query": "rust tokio spawn",
            "count": 5,
            "deep": true
        }))
        .await;
        assert!(search_out.ok, "search failed: {}", search_out.output);
        println!("=== LIVE META-SEARCH OUTPUT ===\n{}\n", search_out.output);
        assert!(search_out.output.contains("tokio"));
        assert!(search_out.output.contains("## Deep Page Excerpts"));

        // Now fetch a specific section from tokio tutorial (should hit the local LRU cache in 0ms!).
        let fetch_out = fetch(
            &json!({
                "url": "https://tokio.rs/tokio/tutorial/spawning",
                "query": "JoinHandle await"
            }),
            &[],
        )
        .await;
        assert!(fetch_out.ok, "fetch failed: {}", fetch_out.output);
        println!("=== LIVE WEBFETCH CACHED QUERY OUTPUT ===\n{}\n", fetch_out.output);
        assert!(fetch_out.output.contains("JoinHandle"));
    }
}
