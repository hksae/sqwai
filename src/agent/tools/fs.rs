#![allow(dead_code)]
//! File tool handlers. Every path passes through `ToolCtx::resolve`
//! (project-jail), mutations snapshot first, edits require a prior read.

use super::{FileDiff, Kind, Outcome, ToolCtx};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

const READ_MAX_LINES: usize = 2000;

fn err<T>(msg: impl Into<String>) -> Result<T, String> {
    Err(msg.into())
}

/// resolve + require existence for read-like ops
fn existing(ctx: &ToolCtx, p: &str) -> Result<PathBuf, String> {
    let p = ctx.resolve(p)?;
    if !p.exists() {
        return err(format!("file not found: {}", p.display()));
    }
    Ok(p)
}

/// guard shared by write-over-existing / edit / multi_edit
fn require_read(ctx: &ToolCtx, p: &Path) -> Result<(), String> {
    match ctx.read_state(p) {
        super::ReadState::Current => Ok(()),
        super::ReadState::Unread => err(format!(
            "edit denied: {} was not read in this session — call read first",
            p.display()
        )),
        super::ReadState::Stale => err(format!(
            "edit denied: {} changed since you read it — read it again before editing, \
             or the edit will be based on content that is gone",
            p.display()
        )),
    }
}

fn is_binary(buf: &[u8]) -> bool {
    buf.iter().take(8000).any(|&b| b == 0)
}

/// unified diff of two file contents (design §4.1: edits are shown to the user
/// after the fact, no confirmation dialog beforehand)
pub(crate) fn make_diff(old: &str, new: &str) -> String {
    let d = similar::TextDiff::from_lines(old, new);
    let out = d
        .unified_diff()
        .context_radius(2)
        .header("before", "after")
        .to_string();
    const MAX_DIFF_LINES: usize = 400;
    let lines: Vec<&str> = out.lines().collect();
    if lines.len() > MAX_DIFF_LINES {
        let head = lines[..MAX_DIFF_LINES].join("\n");
        return format!("{head}\n… diff truncated ({} lines)", lines.len());
    }
    out
}

/// (+added/-removed) line counts from a unified diff body
pub(crate) fn content_hash(content: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(content))
}

pub(crate) fn file_diff(
    path: &Path,
    root: &Path,
    before: Option<&[u8]>,
    after: &[u8],
    mode: &str,
    checkpoint: Option<String>,
    diff: &str,
) -> FileDiff {
    let (added, removed) = diff_counts(diff);
    // Layer 1 (§2.5): keep the bytes this edit is about to replace, so a
    // single step can be reverted without a tree snapshot and without git.
    // A store that cannot be written must not block the edit — the same
    // decision the git snapshot already makes a few lines up.
    let blobs = crate::agent::blobs::dir(root);
    let blob_before = before.and_then(|bytes| crate::agent::blobs::put(root, bytes).ok());
    let blob_after = crate::agent::blobs::put(root, after).ok();
    if blob_before.is_none() && before.is_some() {
        crate::providers::log_http(&format!(
            "blob store unavailable at {}: this edit is not revertible on its own",
            blobs.display()
        ));
    }
    FileDiff {
        path: rel_label(root, path),
        added,
        removed,
        hash_before: before.map(content_hash),
        hash_after: content_hash(after),
        mode: mode.to_string(),
        checkpoint,
        blob_before,
        blob_after,
    }
}

pub(crate) fn diff_counts(diff: &str) -> (usize, usize) {
    let mut add = 0usize;
    let mut rem = 0usize;
    for l in diff.lines().skip(2) {
        // skip the ---/+++ headers
        if let Some(rest) = l.strip_prefix('+') {
            if !rest.starts_with("++") {
                add += 1;
            }
        } else if let Some(rest) = l.strip_prefix('-')
            && !rest.starts_with("--")
        {
            rem += 1;
        }
    }
    (add, rem)
}

pub(super) fn read(ctx: &mut ToolCtx, raw: &str, args: &serde_json::Value) -> Outcome {
    let p = match existing(ctx, raw) {
        Ok(p) => p,
        Err(e) => return Outcome::err(e),
    };
    if !p.is_file() {
        return Outcome::err(format!("not a file: {}", p.display()));
    }
    let bytes = match fs::read(&p) {
        Ok(b) => b,
        Err(e) => return Outcome::err(format!("read failed: {e}")),
    };
    if is_binary(&bytes) {
        return Outcome::err("binary file — cannot display");
    }
    let text = String::from_utf8_lossy(&bytes);
    let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = args["limit"].as_u64().unwrap_or(READ_MAX_LINES as u64) as usize;
    let mut out = String::new();
    for (emitted, (i, line)) in text.lines().enumerate().skip(offset - 1).enumerate() {
        if emitted >= limit.min(READ_MAX_LINES) || out.len() > 300_000 {
            out.push_str("\n…(output truncated)");
            break;
        }
        out.push_str(&format!("{:>6}\t{line}\n", i + 1));
    }
    if text.lines().count() == 0 {
        out.push_str("(empty file)\n");
    }
    ctx.mark_read(&p);
    Outcome::ok(out)
}

pub(super) fn write_file(ctx: &mut ToolCtx, raw: &str, content: &str) -> Outcome {
    let exists = {
        // probe without failing when missing
        ctx.resolve(raw).map(|p| p.exists()).unwrap_or(false)
    };
    // previous contents, kept for the post-facto diff
    let mut prev: Option<String> = None;
    if exists {
        let p = match existing(ctx, raw) {
            Ok(p) => p,
            Err(e) => return Outcome::err(e),
        };
        if let Err(e) = require_read(ctx, &p) {
            return Outcome::err(e);
        }
        prev = fs::read_to_string(&p).ok();
    }
    let p = match ctx.resolve(raw) {
        Ok(p) => p,
        Err(e) => return Outcome::err(e),
    };
    if let Some(parent) = p.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        return Outcome::err(format!("mkdir failed: {e}"));
    }
    // checkpoint before mutation
    checkpoint(ctx, &p, "write");
    if let Err(e) = fs::write(&p, content) {
        return Outcome::err(format!("write failed: {e}"));
    }
    ctx.mark_read(&p);
    let label = rel_label(&ctx.root, &p);
    let diff = prev
        .as_deref()
        .map(|before| make_diff(before, content))
        .unwrap_or_default();
    let checkpoint = ctx.journal.last().map(|(sha, _)| sha.clone());
    let metadata = file_diff(
        &p,
        &ctx.root,
        prev.as_deref().map(str::as_bytes),
        content.as_bytes(),
        "write",
        checkpoint,
        &diff,
    );
    match prev {
        Some(_) => Outcome::ok(format!(
            "wrote {label} (+{}/-{})",
            metadata.added, metadata.removed
        ))
        .with_diff(diff)
        .with_file_diff(metadata),
        None => Outcome::ok(format!(
            "created {} ({} lines)",
            label,
            content.lines().count()
        ))
        .with_file_diff(metadata),
    }
}

fn normalize_to_crlf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\n', "\r\n")
}

fn normalize_to_lf(s: &str) -> String {
    s.replace("\r\n", "\n")
}

fn apply_one(content: &str, old: &str, new: &str, replace_all: bool) -> Result<String, String> {
    if old.is_empty() {
        return err("old_string must not be empty");
    }
    let (target_old, target_new) = if content.matches(old).count() > 0 {
        (std::borrow::Cow::Borrowed(old), std::borrow::Cow::Borrowed(new))
    } else {
        let crlf_old = normalize_to_crlf(old);
        if content.matches(&crlf_old).count() > 0 {
            (
                std::borrow::Cow::Owned(crlf_old),
                std::borrow::Cow::Owned(normalize_to_crlf(new)),
            )
        } else {
            let lf_old = normalize_to_lf(old);
            if content.matches(&lf_old).count() > 0 {
                (
                    std::borrow::Cow::Owned(lf_old),
                    std::borrow::Cow::Owned(normalize_to_lf(new)),
                )
            } else {
                (
                    std::borrow::Cow::Borrowed(old),
                    std::borrow::Cow::Borrowed(new),
                )
            }
        }
    };

    let count = content.matches(target_old.as_ref()).count();
    if count == 0 {
        return err("old_string not found in file");
    }
    if count > 1 && !replace_all {
        return err(format!(
            "old_string appears {count} times — provide more surrounding context or set replace_all=true"
        ));
    }
    Ok(if replace_all {
        content.replace(target_old.as_ref(), target_new.as_ref())
    } else {
        content.replacen(target_old.as_ref(), target_new.as_ref(), 1)
    })
}

pub(super) fn edit(
    ctx: &mut ToolCtx,
    raw: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Outcome {
    let p = match existing(ctx, raw) {
        Ok(p) => p,
        Err(e) => return Outcome::err(e),
    };
    if let Err(e) = require_read(ctx, &p) {
        return Outcome::err(e);
    }
    let content = match fs::read_to_string(&p) {
        Ok(c) => c,
        Err(e) => return Outcome::err(format!("read failed: {e}")),
    };
    let updated = match apply_one(&content, old, new, replace_all) {
        Ok(u) => u,
        Err(e) => return Outcome::err(e),
    };
    checkpoint(ctx, &p, "edit");
    let checkpoint_id = ctx.journal.last().map(|(sha, _)| sha.clone());
    if let Err(e) = fs::write(&p, &updated) {
        return Outcome::err(format!("write failed: {e}"));
    }
    ctx.mark_read(&p);
    let diff = make_diff(&content, &updated);
    let (add, rem) = diff_counts(&diff);
    let metadata = file_diff(
        &p,
        &ctx.root,
        Some(content.as_bytes()),
        updated.as_bytes(),
        "edit",
        checkpoint_id,
        &diff,
    );
    Outcome::ok(format!(
        "edited {} (+{add}/-{rem})",
        rel_label(&ctx.root, &p)
    ))
    .with_diff(diff)
    .with_file_diff(metadata)
}

pub(super) fn multi_edit(
    ctx: &mut ToolCtx,
    raw: &str,
    edits: &[(String, String, bool)],
) -> Outcome {
    let p = match existing(ctx, raw) {
        Ok(p) => p,
        Err(e) => return Outcome::err(e),
    };
    if let Err(e) = require_read(ctx, &p) {
        return Outcome::err(e);
    }
    let mut content = match fs::read_to_string(&p) {
        Ok(c) => c,
        Err(e) => return Outcome::err(format!("read failed: {e}")),
    };
    // validate all replacements against the evolving text before touching disk
    let mut staged = content.clone();
    for (i, (old, new, all)) in edits.iter().enumerate() {
        if let Err(e) = apply_one(&staged, old, new, *all) {
            return Outcome::err(format!("edit #{} failed: {e}", i + 1));
        }
        staged = apply_one(&staged, old, new, *all).unwrap_or(staged.clone());
    }
    checkpoint(ctx, &p, "multi_edit");
    let checkpoint_id = ctx.journal.last().map(|(sha, _)| sha.clone());
    let before = content;
    content = staged;
    if let Err(e) = fs::write(&p, &content) {
        return Outcome::err(format!("write failed: {e}"));
    }
    ctx.mark_read(&p);
    let diff = make_diff(&before, &content);
    let (add, rem) = diff_counts(&diff);
    let metadata = file_diff(
        &p,
        &ctx.root,
        Some(before.as_bytes()),
        content.as_bytes(),
        "multi_edit",
        checkpoint_id,
        &diff,
    );
    Outcome::ok(format!(
        "applied {} edit(s) to {} (+{add}/-{rem})",
        edits.len(),
        rel_label(&ctx.root, &p)
    ))
    .with_diff(diff)
    .with_file_diff(metadata)
}

pub(super) fn ls(ctx: &mut ToolCtx, raw: &str) -> Outcome {
    let p = match ctx.resolve(raw) {
        Ok(p) => p,
        Err(e) => return Outcome::err(e),
    };
    if !p.is_dir() {
        return Outcome::err(format!("not a directory: {}", p.display()));
    }
    let mut rows: Vec<(bool, u64, String)> = Vec::new();
    let rd = match fs::read_dir(&p) {
        Ok(r) => r,
        Err(e) => return Outcome::err(format!("readdir failed: {e}")),
    };
    for e in rd.flatten() {
        let meta = e.metadata().ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        rows.push((is_dir, size, e.file_name().to_string_lossy().into_owned()));
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.cmp(&b.2)));
    let _ = Kind::ReadOnly; // kind metadata lives in the registry
    let body: Vec<String> = rows
        .iter()
        .map(|(d, s, n)| {
            if *d {
                format!("{n}/")
            } else {
                format!("{n} ({s} B)")
            }
        })
        .collect();
    Outcome::ok(if body.is_empty() {
        "(empty directory)".into()
    } else {
        body.join("\n")
    })
}

pub(super) fn glob(ctx: &mut ToolCtx, pattern: &str, base: Option<&str>) -> Outcome {
    use globset::GlobBuilder;
    use ignore::WalkBuilder;

    let base_dir = match base {
        Some(b) => match ctx.resolve(b) {
            Ok(p) => p,
            Err(e) => return Outcome::err(e),
        },
        None => ctx.root.clone(),
    };
    let glob = match GlobBuilder::new(pattern).literal_separator(true).build() {
        Ok(g) => g.compile_matcher(),
        Err(e) => return Outcome::err(format!("bad glob pattern: {e}")),
    };
    let mut hits: Vec<String> = Vec::new();
    for entry in WalkBuilder::new(&base_dir).hidden(true).build().flatten() {
        if hits.len() >= 300 {
            hits.push("…(more results truncated)".into());
            break;
        }
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let rel = path.strip_prefix(&base_dir).unwrap_or(path);
        if glob.is_match(rel) {
            hits.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Outcome::ok(if hits.is_empty() {
        "no matches".into()
    } else {
        hits.join("\n")
    })
}

pub(super) fn grep(
    ctx: &mut ToolCtx,
    pattern: &str,
    path: Option<&str>,
    include: Option<&str>,
) -> Outcome {
    use ignore::WalkBuilder;
    use std::io::BufRead;

    let re = match regex::Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => return Outcome::err(format!("bad regex: {e}")),
    };
    let base_dir = match path {
        Some(b) => match ctx.resolve(b) {
            Ok(p) => p,
            Err(e) => return Outcome::err(e),
        },
        None => ctx.root.clone(),
    };
    let inc = match include {
        Some(g) => match globset::GlobBuilder::new(g).build() {
            Ok(gb) => Some(gb.compile_matcher()),
            Err(e) => return Outcome::err(format!("bad include glob pattern: {e}")),
        },
        None => None,
    };

    let mut out = String::new();
    let mut matches = 0usize;
    'outer: for entry in WalkBuilder::new(&base_dir).hidden(true).build().flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Some(f) = &inc {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !f.is_match(&name) {
                continue;
            }
        }
        let file = match fs::File::open(path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let rd = std::io::BufReader::new(file);
        for (i, line) in rd.lines().enumerate() {
            let Ok(line) = line else { break };
            if line.contains('\0') {
                continue 'outer; // binary-ish
            }
            if re.is_match(&line) {
                matches += 1;
                let disp = path.strip_prefix(&ctx.root).unwrap_or(path);
                let shown = disp.display().to_string().replace('\\', "/");
                let trimmed = line.trim_end();
                const MAX_LINE_BYTES: usize = 1024;
                let display_line = if trimmed.len() > MAX_LINE_BYTES {
                    let mut cut = MAX_LINE_BYTES;
                    while cut > 0 && !trimmed.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    format!("{}…(line truncated)", &trimmed[..cut])
                } else {
                    trimmed.to_string()
                };
                out.push_str(&format!("{}:{}: {}\n", shown, i + 1, display_line));
                if matches >= 200 {
                    out.push_str("…(more matches truncated)\n");
                    break 'outer;
                }
            }
        }
    }
    Outcome::ok(if out.is_empty() {
        json_out_no_matches(pattern)
    } else {
        out
    })
}

fn json_out_no_matches(pattern: &str) -> String {
    format!("no matches for /{pattern}/")
}

fn checkpoint(ctx: &mut ToolCtx, p: &Path, what: &str) {
    let label = format!("{what} {}", rel_label(&ctx.root, p));
    // Layer 2 is best-effort by design: layer 1 already holds this file's
    // pre-image, so a missing git binary costs the tree snapshot and nothing
    // else (§2.5's degradation table).
    // An unchanged tree needs no commit, and no git binary means no layer 2 —
    // neither is a reason to fail the edit.
    if let Ok(Some(sha)) = crate::agent::checkpoints::snapshot_session(
        &ctx.root,
        ctx.shadow_store,
        &ctx.session_id,
        &label,
    ) {
        ctx.journal.push((sha, label));
    }
}

fn rel_label(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

// keep json import used even if helpers change
#[allow(unused)]
fn _touch() -> serde_json::Value {
    json!(null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consecutive_edits_do_not_require_rereading() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, "hello world").unwrap();

        let mut ctx = ToolCtx::new(dir.path());
        let read_outcome = read(&mut ctx, "test.txt", &json!({}));
        assert!(read_outcome.ok);

        let edit1 = edit(&mut ctx, "test.txt", "world", "rust", false);
        assert!(edit1.ok, "first edit failed: {}", edit1.output);

        let edit2 = edit(&mut ctx, "test.txt", "rust", "sqwai", false);
        assert!(edit2.ok, "second edit failed: {}", edit2.output);
    }

    #[test]
    fn grep_invalid_include_glob_returns_error_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolCtx::new(dir.path());
        let outcome = grep(&mut ctx, "test", None, Some("[unclosed"));
        assert!(!outcome.ok);
        assert!(outcome.output.contains("bad include glob pattern"));
    }

    #[test]
    fn grep_long_line_is_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let long_line = format!("match_{}", "a".repeat(3000));
        std::fs::write(dir.path().join("long.txt"), &long_line).unwrap();
        let mut ctx = ToolCtx::new(dir.path());
        let outcome = grep(&mut ctx, "match_", None, None);
        assert!(outcome.ok);
        assert!(outcome.output.contains("…(line truncated)"));
        assert!(outcome.output.len() < 2000);
    }

    #[test]
    fn apply_one_crlf_matching() {
        // File has CRLF, replacement pattern has LF
        let crlf_content = "line1\r\nline2\r\nline3\r\n";
        let res = apply_one(crlf_content, "line2\n", "modified\n", false).unwrap();
        assert_eq!(res, "line1\r\nmodified\r\nline3\r\n");

        // Multi-line replacement
        let res_multi = apply_one(crlf_content, "line1\nline2\n", "replaced\n", false).unwrap();
        assert_eq!(res_multi, "replaced\r\nline3\r\n");

        // File has LF, replacement pattern has CRLF
        let lf_content = "line1\nline2\nline3\n";
        let res_lf = apply_one(lf_content, "line2\r\n", "modified\r\n", false).unwrap();
        assert_eq!(res_lf, "line1\nmodified\nline3\n");
    }
}
