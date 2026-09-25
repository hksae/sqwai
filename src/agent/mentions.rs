//! @-mentions: user-typed `file:`/`sym:` references (or bare paths and
//! names) resolved to bytes at send time.
//!
//! Design (§2.4.10, P3.2):
//! - The composer inserts canonical keys (`@file:src/main.rs`,
//!   `@sym:src/main.rs::Config`); the send path resolves them here.
//! - Resolution reads from disk at submit, so the bytes are always fresh
//!   (stale content cannot be injected — there is nothing older to pin).
//! - The fence carries the sha256 of exactly the injected bytes plus the
//!   canonical key, so the transcript shows what the model saw.
//! - Resolved files seed the read guard (same hash the `read` tool
//!   records), so an edit after a mention works without a redundant read.
//! - Unresolvable tokens stay literal with a warning — never a guess.
//! - NO auto-neighbours, NO auto-binding to plan refs: only what the user
//!   named travels.

use std::path::{Path, PathBuf};

use crate::agent::graph::GraphStore;

/// Whole-file inject cap: beyond this the fence truncates with a marker.
/// The hash always covers the injected bytes, never the whole file.
pub const MENTION_MAX_LINES: usize = 200;

/// One `@` token in the text: byte offsets plus the raw key (no `@`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionToken {
    pub start: usize,
    pub end: usize,
    pub raw: String,
}

/// Outcome of resolving a message: the model-bound text, canonical paths
/// to seed the read guard with, and warnings for kept-literal tokens.
#[derive(Debug, Default)]
pub struct ResolvedText {
    pub text: String,
    pub pre_reads: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

fn is_token_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | '/' | ':' | '@' | '-')
}

/// Byte offset where the `@` token starting at `at` ends, sans trailing
/// sentence punctuation (`@src/main.rs.` names the file, not the dot).
fn token_end(text: &str, at: usize) -> usize {
    let mut end = at + 1;
    for (idx, c) in text[at + 1..].char_indices() {
        if is_token_char(c) {
            end = at + 1 + idx + c.len_utf8();
        } else {
            break;
        }
    }
    while end > at + 1 {
        let c = text[..end].chars().next_back().unwrap_or('x');
        if matches!(c, '.' | ',' | ';' | '!' | '?') {
            end -= c.len_utf8();
        } else {
            break;
        }
    }
    end
}

/// The `@` token containing byte offset `cursor`, if any. `@` opens a
/// token only at a word start (text start, whitespace, bracket or quote),
/// so `user@host` mail addresses never count.
pub fn mention_at(text: &str, cursor: usize) -> Option<MentionToken> {
    let cursor = cursor.min(text.len());
    let mut at = None;
    let mut idx = cursor;
    while idx > 0 {
        let c = text[..idx].chars().next_back()?;
        if c == '@' {
            at = Some(idx - c.len_utf8());
            break;
        }
        if !is_token_char(c) {
            break;
        }
        idx -= c.len_utf8();
    }
    let at = at?;
    if at > 0 {
        let prev = text[..at].chars().next_back()?;
        if !(prev.is_whitespace() || matches!(prev, '(' | '"' | '\'' | '[')) {
            return None;
        }
    }
    let end = token_end(text, at);
    if end <= at + 1 || cursor < at || cursor > end {
        return None;
    }
    Some(MentionToken {
        start: at,
        end,
        raw: text[at + 1..end].to_string(),
    })
}

/// Every `@` token in the text, in order, non-overlapping.
pub fn find_mentions(text: &str) -> Vec<MentionToken> {
    let mut out = Vec::new();
    let mut idx = 0;
    while idx < text.len() {
        let Some(rel) = text[idx..].find('@') else {
            break;
        };
        let at = idx + rel;
        if at > 0 {
            let prev = text[..at].chars().next_back();
            if let Some(prev) = prev
                && !(prev.is_whitespace() || matches!(prev, '(' | '"' | '\'' | '['))
            {
                idx = at + 1;
                continue;
            }
        }
        let end = token_end(text, at);
        if end > at + 1 {
            out.push(MentionToken {
                start: at,
                end,
                raw: text[at + 1..end].to_string(),
            });
            idx = end;
        } else {
            idx = at + 1;
        }
    }
    out
}

/// Split a trailing `:N` / `:N-M` range suffix (1-based, inclusive).
/// Anything else — including a bare numberless tail — is not a range.
/// Exposed for completion filtering (a half-typed range still matches).
pub(crate) fn split_range(raw: &str) -> (String, Option<(usize, usize)>) {
    fn trailing_digits(s: &str) -> usize {
        s.bytes().rev().take_while(|b| b.is_ascii_digit()).count()
    }
    let none = || (raw.to_string(), None);
    let nd = trailing_digits(raw);
    if nd == 0 {
        return none();
    }
    let last: usize = match raw[raw.len() - nd..].parse() {
        Ok(n) if n > 0 => n,
        _ => return none(),
    };
    let mut head = &raw[..raw.len() - nd];
    let mut first = last;
    if let Some(h) = head.strip_suffix('-') {
        let fd = trailing_digits(h);
        if fd == 0 || fd == h.len() {
            return none();
        }
        let hh = &h[..h.len() - fd];
        let Some(hh) = hh.strip_suffix(':') else {
            return none();
        };
        if hh.is_empty() {
            return none();
        }
        first = match h[h.len() - fd..].parse() {
            Ok(n) if n > 0 => n,
            _ => return none(),
        };
        head = hh;
    } else {
        let Some(h) = head.strip_suffix(':') else {
            return none();
        };
        if h.is_empty() {
            return none();
        }
        head = h;
    }
    if first > last {
        return none();
    }
    (head.to_string(), Some((first, last)))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for b in hasher.finalize() {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 15) as usize] as char);
    }
    out
}

fn lang_of(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "rs" => "rust",
        "py" => "python",
        "js" => "javascript",
        "ts" => "typescript",
        "tsx" => "tsx",
        "jsx" => "jsx",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "hpp" | "cc" => "cpp",
        "cs" => "csharp",
        "rb" => "ruby",
        "php" => "php",
        "swift" => "swift",
        "kt" => "kotlin",
        "sh" => "bash",
        "ps1" => "powershell",
        "sql" => "sql",
        "md" | "markdown" => "markdown",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "json" => "json",
        "html" | "htm" => "html",
        "css" => "css",
        _ => "",
    }
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|&b| b == 0)
}

/// Resolve one token against the project. Returns the fence plus the
/// absolute path for read-guard seeding, or a warning when the token
/// stays literal.
fn resolve_token(
    ctx: &crate::agent::tools::ToolCtx,
    store: Option<&crate::agent::graph::SqliteGraphStore>,
    raw: &str,
) -> Result<(String, PathBuf), String> {
    let raw = raw.replace('\\', "/");
    let (head, range) = split_range(&raw);
    if let Some(path) = head.strip_prefix("file:") {
        if path.is_empty() {
            return Err(format!("@{raw}: empty file path, kept literally"));
        }
        return read_target(ctx, path, range, &format!("file:{path}"), &raw);
    }
    if let Some(rest) = head.strip_prefix("sym:") {
        let Some(store) = store else {
            return Err(format!("@{raw}: graph index unavailable, kept literally"));
        };
        let key = format!("sym:{rest}");
        let node = store
            .find_node(&key)
            .ok()
            .flatten()
            .ok_or_else(|| format!("@{raw}: unknown symbol, kept literally"))?;
        let node_path = node.path.clone().unwrap_or_default();
        if node_path.is_empty() {
            return Err(format!("@{raw}: symbol has no file, kept literally"));
        }
        return read_target(ctx, &node_path, range.or(node_lines(&node)), &key, &raw);
    }
    // bare token, smart: an existing file wins, else an exact graph hit
    if ctx.resolve(&head).is_ok_and(|abs| abs.is_file()) {
        return read_target(ctx, &head, range, &format!("file:{head}"), &raw);
    }
    if let Some(store) = store {
        let hits = store.recall(&head, 5).unwrap_or_default();
        if let Some(hit) = hits
            .iter()
            .find(|h| h.score >= 0.9 && h.key.starts_with("sym:"))
            && let Some(node) = store.find_node(&hit.key).ok().flatten()
            && let Some(node_path) = node.path.clone()
            && !node_path.is_empty()
        {
            return read_target(ctx, &node_path, range.or(node_lines(&node)), &hit.key, &raw);
        }
    }
    Err(format!("@{raw}: unresolved mention, kept literally"))
}

fn node_lines(node: &crate::agent::graph::Node) -> Option<(usize, usize)> {
    match (node.line_start, node.line_end) {
        (Some(s), Some(e)) if s >= 1 && e >= s => Some((s as usize, e as usize)),
        (Some(s), _) if s >= 1 => Some((s as usize, s as usize)),
        _ => None,
    }
}

/// Read, slice, fence and hash one file target. The hash covers exactly
/// the injected bytes. Explicit ranges bypass the whole-file cap (the
/// user asked for exactly these lines); whole files truncate.
fn read_target(
    ctx: &crate::agent::tools::ToolCtx,
    path: &str,
    range: Option<(usize, usize)>,
    canonical_key: &str,
    raw: &str,
) -> Result<(String, PathBuf), String> {
    let abs = ctx
        .resolve(path)
        .map_err(|e| format!("@{raw}: {e}, kept literally"))?;
    if !abs.is_file() {
        return Err(format!("@{raw}: no such file, kept literally"));
    }
    let bytes =
        std::fs::read(&abs).map_err(|e| format!("@{raw}: unreadable ({e}), kept literally"))?;
    if is_binary(&bytes) {
        return Err(format!("@{raw}: binary file, kept literally"));
    }
    let text = String::from_utf8_lossy(&bytes);
    let all: Vec<&str> = text.lines().collect();
    let total = all.len();
    let (from, to, capped) = match range {
        Some((s, e)) => {
            if s > total {
                return Err(format!(
                    "@{raw}: range starts past end of file ({total} lines), kept literally"
                ));
            }
            (s, e.min(total), false)
        }
        None if total > MENTION_MAX_LINES => (1, MENTION_MAX_LINES, true),
        None => (1, total, false),
    };
    if from > to {
        return Err(format!("@{raw}: empty range, kept literally"));
    }
    let slice = all[from - 1..to].join("\n");
    let hash = sha256_hex(slice.as_bytes());
    let short = &hash[..12.min(hash.len())];
    let mut fence = format!(
        "@{canonical_key}#sha256:{short} (lines {from}-{to} of {total})\n```{}\n{slice}\n```",
        lang_of(canonical_key)
    );
    if capped {
        fence.push_str(&format!("\n…({} more lines truncated)", total - to));
    }
    Ok((fence, abs))
}

/// Resolve every `@` token in `text` against `root`: model-bound text
/// with fenced blocks, canonical paths for read-guard seeding, warnings
/// for kept-literal tokens. Never fails the turn — worst case the text
/// passes through with warnings.
pub fn resolve_mentions(root: &Path, text: &str) -> ResolvedText {
    let mut out = ResolvedText {
        text: String::with_capacity(text.len()),
        ..Default::default()
    };
    let tokens = find_mentions(text);
    if tokens.is_empty() {
        out.text = text.to_string();
        return out;
    }
    let ctx = crate::agent::tools::ToolCtx::new(root);
    let store = crate::agent::graph::SqliteGraphStore::open(root).ok();
    let store_ref = store.as_ref();
    let mut cursor = 0;
    for token in tokens {
        out.text.push_str(&text[cursor..token.start]);
        cursor = token.end;
        match resolve_token(&ctx, store_ref, &token.raw) {
            Ok((fence, abs)) => {
                out.text.push_str(&fence);
                if !out.pre_reads.contains(&abs) {
                    out.pre_reads.push(abs);
                }
            }
            Err(warning) => {
                out.text.push('@');
                out.text.push_str(&token.raw);
                out.warnings.push(warning);
            }
        }
    }
    out.text.push_str(&text[cursor..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_need_a_word_start() {
        assert!(find_mentions("mail user@host x").is_empty());
        let found = find_mentions("see @src/main.rs ok");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].raw, "src/main.rs");
        // sentence punctuation is not part of the key
        let found = find_mentions("see @src/main.rs.");
        assert_eq!(found[0].raw, "src/main.rs");
        // quoted operators are not tokens either
        assert!(find_mentions("echo \"a > b\"").is_empty());
    }

    #[test]
    fn mention_at_finds_the_token_under_the_cursor() {
        let text = "see @src/main.rs ok";
        let token = mention_at(text, 9).expect("on the key");
        assert_eq!(token.raw, "src/main.rs");
        assert!(mention_at(text, 4).is_none(), "before the @");
        assert!(mention_at(text, text.len()).is_none(), "past the token");
        assert!(mention_at("user@host", 5).is_none());
    }

    #[test]
    fn split_range_accepts_only_sane_suffixes() {
        assert_eq!(split_range("src/main.rs"), ("src/main.rs".into(), None));
        assert_eq!(split_range("f:10"), ("f".into(), Some((10, 10))));
        assert_eq!(split_range("f:10-20"), ("f".into(), Some((10, 20))));
        assert_eq!(split_range("sym:p::n:3"), ("sym:p::n".into(), Some((3, 3))));
        for bad in ["f:0", "f:20-10", "f:", "f:-3", ":10", "2130706433", "a:b", "f:1x"] {
            assert_eq!(split_range(bad), (bad.into(), None), "{bad}");
        }
    }

    fn proj(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
        }
        dir
    }

    #[test]
    fn resolve_injects_fenced_bytes_with_a_matching_pin() {
        let dir = proj(&[("src/main.rs", "fn main() {}\n// two\n")]);
        let resolved = resolve_mentions(dir.path(), "look at @file:src/main.rs please");
        assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);
        assert!(resolved.text.contains("@file:src/main.rs#sha256:"));
        assert!(resolved.text.contains("```rust\nfn main() {}\n// two\n```"));
        assert!(resolved.text.contains("(lines 1-2 of 2)"));
        assert_eq!(resolved.pre_reads.len(), 1);
        // the pin covers exactly the injected bytes
        let body = resolved.text.split("```rust\n").nth(1).unwrap();
        let body = body.split("\n```").next().unwrap();
        let pin_at = resolved.text.find("#sha256:").unwrap() + "#sha256:".len();
        assert_eq!(&sha256_hex(body.as_bytes())[..12], &resolved.text[pin_at..pin_at + 12]);
        std::fs::remove_dir_all(dir.path()).ok();
    }

    #[test]
    fn resolve_ranges_caps_and_warns() {
        let content = (1..=10).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let dir = proj(&[("a.txt", &content)]);
        // explicit range bypasses the cap
        let resolved = resolve_mentions(dir.path(), "@file:a.txt:3-5");
        assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);
        assert!(resolved.text.contains("(lines 3-5 of 10)"));
        assert!(resolved.text.contains("line 3\nline 4\nline 5"));
        // past-end range stays literal with a warning
        let resolved = resolve_mentions(dir.path(), "@file:a.txt:99");
        assert_eq!(resolved.text, "@file:a.txt:99");
        assert_eq!(resolved.warnings.len(), 1);
        // missing file stays literal with a warning
        let resolved = resolve_mentions(dir.path(), "@nope.rs and @file:nope.rs");
        assert!(resolved.text.contains("@nope.rs"));
        assert!(resolved.text.contains("@file:nope.rs"));
        assert_eq!(resolved.warnings.len(), 2);
        assert!(resolved.pre_reads.is_empty());
        std::fs::remove_dir_all(dir.path()).ok();
    }

    #[test]
    fn resolve_bare_path_prefers_the_file() {
        let dir = proj(&[("src/lib.rs", "lib\n")]);
        let resolved = resolve_mentions(dir.path(), "check @src/lib.rs");
        assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);
        assert!(resolved.text.contains("@file:src/lib.rs#sha256:"));
        std::fs::remove_dir_all(dir.path()).ok();
    }

    #[test]
    fn resolve_refuses_binaries_and_host_state() {
        let dir = proj(&[("b.bin", "a\0b")]);
        std::fs::create_dir_all(dir.path().join(".sqwai")).unwrap();
        std::fs::write(dir.path().join(".sqwai/x.json"), "{}").unwrap();
        let resolved = resolve_mentions(dir.path(), "@file:b.bin and @file:.sqwai/x.json");
        assert_eq!(resolved.text, "@file:b.bin and @file:.sqwai/x.json");
        assert_eq!(resolved.warnings.len(), 2);
        assert!(resolved.pre_reads.is_empty());
        std::fs::remove_dir_all(dir.path()).ok();
    }
}
