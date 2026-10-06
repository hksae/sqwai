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

/// Spilled tool output: oversized results land in a gitignored sidecar dir
/// instead of dying in a truncation marker. The model gets the path plus a
/// standing instruction to grep it or page it — never to re-read it whole.
pub(super) fn spill_output(root: &Path, tool: &str, text: &str) -> Option<String> {
    const RETENTION_SECS: u64 = 7 * 24 * 3600;
    static SPILL_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = root.join("sqwai-spill");
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    // best-effort retention: drop week-old spills on every write
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let now = std::time::SystemTime::now();
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|mtime| now.duration_since(mtime).ok())
                .is_some_and(|age| age.as_secs() > RETENTION_SECS);
            if old {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let id = SPILL_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = format!("{tool}-{}-{id}.txt", std::process::id());
    let path = dir.join(&name);
    if std::fs::write(&path, text).is_err() {
        return None;
    }
    Some(format!("sqwai-spill/{name}"))
}

/// guard shared by destructive writes: overwriting an existing file and
/// replace_all edits proceed only on a fresh read — blind mass changes
/// are the one failure exact matching cannot catch.
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

/// freshness-only gate for self-validating writes: single exact edits and
/// atomic multi_edits match-or-error before touching disk, so they need no
/// prior read — but working from a stale read is still refused, stale
/// knowledge pointing at moved code.
fn require_fresh(ctx: &ToolCtx, p: &Path) -> Result<(), String> {
    match ctx.read_state(p) {
        super::ReadState::Stale => err(format!(
            "edit denied: {} changed since you read it — read it again before editing, \
             or the edit will be based on content that is gone",
            p.display()
        )),
        super::ReadState::Current | super::ReadState::Unread => Ok(()),
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
    let bytes = match fs::read(&p) {
        Ok(b) => b,
        Err(e) => return Outcome::err(format!("read failed: {e}")),
    };
    if is_binary(&bytes) {
        return Outcome::err("binary file — cannot display");
    }
    let text = String::from_utf8_lossy(&bytes);
    let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit =
        (args["limit"].as_u64().unwrap_or(READ_MAX_LINES as u64) as usize).min(READ_MAX_LINES);
    // repeat reads of an unchanged window collapse to a stub: re-reading
    // the same files in circles was the top context burner, and the
    // freshness guard (require_read) already forces a fresh read when the
    // file actually moved.
    let window_hash = content_hash(&bytes);
    if ctx.read_window_hit(&p, offset, limit, &window_hash) {
        return Outcome::ok(format!(
            "…(unchanged since your last read of this window: {} lines {}+, see above — no need to re-read)",
            rel_label(&ctx.root, &p),
            offset,
        ));
    }
    let total_lines = text.lines().count();
    let mut out = String::new();
    let first_no = offset;
    let mut last_no = offset.saturating_sub(1);
    let mut truncated = false;
    for (i, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
        if out.len() > 300_000 {
            truncated = true;
            break;
        }
        out.push_str(&format!("{:>6}\t{line}\n", i + 1));
        last_no = i + 1;
    }
    if !truncated && last_no < total_lines {
        truncated = true;
    }
    if total_lines == 0 {
        out.push_str("(empty file)\n");
    } else if truncated {
        match spill_output(&ctx.root, "read", &text) {
            Some(rel) => out.push_str(&format!(
                "\n…(showing lines {first_no}–{last_no} of {total_lines} total — full output saved to {rel}: use grep to search it or read with offset/limit for specific sections, do not re-read the whole file)"
            )),
            None => out.push_str("\n…(output truncated)"),
        }
    } else {
        out.push_str(&format!(
            "\n(showing lines {first_no}–{last_no} of {total_lines} total)"
        ));
    }
    ctx.mark_read(&p);
    ctx.note_read_window(&p, offset, limit, window_hash);
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
    let diff = match prev.as_deref() {
        Some(before) => make_diff(before, content),
        // a created file is all-added: the TUI expanded row and the
        // write-path lints (AF) see the same diff shape as an edit
        None => make_diff("", content),
    };
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
        .with_diff(diff)
        .with_file_diff(metadata),
    }
}

fn normalize_to_crlf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\n', "\r\n")
}

fn normalize_to_lf(s: &str) -> String {
    s.replace("\r\n", "\n")
}

pub(super) fn apply_one(
    content: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<String, String> {
    if old.is_empty() {
        return err("old_string must not be empty");
    }
    let (target_old, target_new) = if content.matches(old).count() > 0 {
        (
            std::borrow::Cow::Borrowed(old),
            std::borrow::Cow::Borrowed(new),
        )
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
    // mass replacement without a fresh read is blind destruction; a single
    // exact edit is self-validating (match-or-error before touching disk)
    // and only needs the file to be non-stale
    if replace_all {
        if let Err(e) = require_read(ctx, &p) {
            return Outcome::err(e);
        }
    } else if let Err(e) = require_fresh(ctx, &p) {
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
    let out_msg = format!("edited {} (+{add}/-{rem})", rel_label(&ctx.root, &p));
    Outcome::ok(out_msg)
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
    if edits.iter().any(|(_, _, all)| *all) {
        if let Err(e) = require_read(ctx, &p) {
            return Outcome::err(e);
        }
    } else if let Err(e) = require_fresh(ctx, &p) {
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
    let out_msg = format!(
        "applied {} edit(s) to {} (+{add}/-{rem})",
        edits.len(),
        rel_label(&ctx.root, &p)
    );
    Outcome::ok(out_msg)
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
    // host-owned state is never listed, whatever the walker thinks about
    // hidden files: on Windows `hidden(true)` checks attributes, not dots,
    // so `.sqwai/` would leak without this explicit skip
    let host_owned = ctx.host_state_dir();
    for entry in WalkBuilder::new(&base_dir).hidden(true).build().flatten() {
        if hits.len() >= 300 {
            hits.push("…(more results truncated)".into());
            break;
        }
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path == host_owned || path.starts_with(&host_owned) {
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
    context: usize,
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

    fn show_line(line: &str) -> String {
        const MAX_LINE_BYTES: usize = 1024;
        let trimmed = line.trim_end();
        if trimmed.len() > MAX_LINE_BYTES {
            let mut cut = MAX_LINE_BYTES;
            while cut > 0 && !trimmed.is_char_boundary(cut) {
                cut -= 1;
            }
            format!("{}…(line truncated)", &trimmed[..cut])
        } else {
            trimmed.to_string()
        }
    }

    let mut out = String::new();
    let mut matches = 0usize;
    // same host-owned skip as glob above (see comment there)
    let host_owned = ctx.root.join(".sqwai");
    'outer: for entry in WalkBuilder::new(&base_dir).hidden(true).build().flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path == host_owned || path.starts_with(&host_owned) {
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
        let lines: Vec<String> = rd.lines().collect::<Result<_, _>>().unwrap_or_default();
        if lines.iter().any(|line| line.contains('\0')) {
            continue; // binary-ish
        }
        let disp = path.strip_prefix(&ctx.root).unwrap_or(path);
        let shown = disp.display().to_string().replace('\\', "/");
        let mut hits: Vec<usize> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if re.is_match(line) {
                hits.push(i);
                matches += 1;
                if matches >= 200 {
                    match spill_output(&ctx.root, "grep", &out) {
                        Some(rel) => out.push_str(&format!(
                            "…(more matches truncated: first 200 shown, full list in {rel} — grep it with a narrower pattern instead of re-running broad)\n"
                        )),
                        None => out.push_str("…(more matches truncated)\n"),
                    }
                    break 'outer;
                }
            }
        }
        // grep -C shape: match lines with `:`, context with `-`, `--` between groups
        let mut last_printed: Option<usize> = None;
        for &hit in &hits {
            let start = hit.saturating_sub(context);
            let end = (hit + context + 1).min(lines.len());
            if let Some(prev) = last_printed
                && start > prev + 1
            {
                out.push_str("--\n");
            }
            for (i, line) in lines.iter().enumerate().take(end).skip(start) {
                if last_printed.is_some_and(|p| i <= p) {
                    continue;
                }
                let sep = if i == hit { ':' } else { '-' };
                out.push_str(&format!("{}{sep}{}: {}\n", shown, i + 1, show_line(line)));
                last_printed = Some(i);
            }
            if out.len() > 300_000 {
                out.push_str("…(output truncated)\n");
                break 'outer;
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
        ctx.checkpoint_chain(),
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
    fn repeat_read_of_unchanged_window_collapses_to_stub() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let mut ctx = ToolCtx::new(dir.path());
        let args = serde_json::json!({});
        let first = read(&mut ctx, "a.txt", &args);
        assert!(first.ok);
        assert!(first.output.contains("one"), "{}", first.output);
        // same window, unchanged file: stub instead of the bytes again
        let second = read(&mut ctx, "a.txt", &args);
        assert!(second.ok);
        assert!(
            second.output.contains("unchanged since your last read"),
            "{}",
            second.output
        );
        // after an edit the window serves fresh again
        let edited = edit(&mut ctx, "a.txt", "two", "TWO", false);
        assert!(edited.ok, "{}", edited.output);
        let third = read(&mut ctx, "a.txt", &args);
        assert!(third.ok);
        assert!(third.output.contains("TWO"), "{}", third.output);
        assert!(
            !third.output.contains("unchanged since your last read"),
            "{}",
            third.output
        );
    }

    #[test]
    fn grep_invalid_include_glob_returns_error_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolCtx::new(dir.path());
        let outcome = grep(&mut ctx, "test", None, Some("[unclosed"), 0);
        assert!(!outcome.ok);
        assert!(outcome.output.contains("bad include glob pattern"));
    }

    #[test]
    fn grep_long_line_is_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let long_line = format!("match_{}", "a".repeat(3000));
        std::fs::write(dir.path().join("long.txt"), &long_line).unwrap();
        let mut ctx = ToolCtx::new(dir.path());
        let outcome = grep(&mut ctx, "match_", None, None, 0);
        assert!(outcome.ok);
        assert!(outcome.output.contains("…(line truncated)"));
        assert!(outcome.output.len() < 2000);
    }

    #[test]
    fn grep_context_shows_surrounding_lines_like_grep_c() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("code.txt"),
            "line one\nline two\nMATCH here\nline four\nline five\n",
        )
        .unwrap();
        let mut ctx = ToolCtx::new(dir.path());
        let outcome = grep(&mut ctx, "MATCH", None, None, 1);
        assert!(outcome.ok);
        // match line with `:`, context lines with `-`
        assert!(
            outcome.output.contains("code.txt:3: MATCH here"),
            "{}",
            outcome.output
        );
        assert!(outcome.output.contains("code.txt-2:"), "{}", outcome.output);
        assert!(outcome.output.contains("code.txt-4:"), "{}", outcome.output);
        // default stays context-free
        let plain = grep(&mut ctx, "MATCH", None, None, 0);
        assert!(plain.ok);
        assert!(!plain.output.contains("code.txt-2:"), "{}", plain.output);
    }

    #[test]
    fn host_owned_state_is_invisible_to_grep_and_glob() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".sqwai").join("plans")).unwrap();
        std::fs::write(
            dir.path().join(".sqwai").join("plans").join("secret.json"),
            "host_secret_marker_xyz",
        )
        .unwrap();
        std::fs::write(dir.path().join("visible.txt"), "plain content").unwrap();
        let mut ctx = ToolCtx::new(dir.path());
        // grep must not match inside host-owned state (pre-fix: leaked on
        // Windows where hidden(true) checks attributes, not dotfiles)
        let outcome = grep(&mut ctx, "host_secret_marker_xyz", None, None, 0);
        assert!(outcome.ok);
        assert!(!outcome.output.contains(".sqwai"), "{}", outcome.output);
        // glob must not list host-owned files either
        let listed = glob(&mut ctx, "**/*", None);
        assert!(listed.ok);
        assert!(!listed.output.contains(".sqwai"), "{}", listed.output);
        assert!(listed.output.contains("visible.txt"), "{}", listed.output);
    }

    #[test]
    fn host_state_dir_is_canonical() {
        // the walk yields canonical paths while the root may sit behind a
        // symlink (macOS `/var` -> `/private/var`): a raw-root prefix check
        // would silently never fire
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolCtx::new(dir.path());
        assert_eq!(
            ctx.host_state_dir(),
            dir.path().canonicalize().unwrap().join(".sqwai")
        );
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

    /// Audit verification (Phase 0): each replacement applies exactly once.
    /// An overlapping `a → ba` on content `a` must yield `ba`, not `bba` —
    /// the validation pass result is discarded, only the second call writes.
    #[test]
    fn multi_edit_applies_each_replacement_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("t.txt"), "a").unwrap();
        let mut ctx = ToolCtx::new(dir.path());
        assert!(read(&mut ctx, "t.txt", &json!({})).ok);
        let out = multi_edit(&mut ctx, "t.txt", &[("a".into(), "ba".into(), false)]);
        assert!(out.ok, "{}", out.output);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("t.txt")).unwrap(),
            "ba"
        );
        // chained edits see each other's results, still once each
        std::fs::write(dir.path().join("u.txt"), "a").unwrap();
        assert!(read(&mut ctx, "u.txt", &json!({})).ok);
        let out = multi_edit(
            &mut ctx,
            "u.txt",
            &[
                ("a".into(), "ba".into(), false),
                ("ba".into(), "cba".into(), false),
            ],
        );
        assert!(out.ok, "{}", out.output);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("u.txt")).unwrap(),
            "cba"
        );
    }
}
