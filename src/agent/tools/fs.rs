#![allow(dead_code)]
//! File tool handlers. Every path passes through `ToolCtx::resolve`
//! (project-jail), mutations snapshot first, edits require a prior read.

use super::{Kind, Outcome, ToolCtx};
use serde_json::json;
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
    if !ctx.was_read(p) {
        return err(format!(
            "edit denied: {} was not read in this session — call read first",
            p.display()
        ));
    }
    Ok(())
}

fn is_binary(buf: &[u8]) -> bool {
    buf.iter().take(8000).any(|&b| b == 0)
}

/// unified diff of two file contents (design §4.1: edits are shown to the user
/// after the fact, no confirmation dialog beforehand)
fn make_diff(old: &str, new: &str) -> String {
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
fn diff_counts(diff: &str) -> (usize, usize) {
    let mut add = 0usize;
    let mut rem = 0usize;
    for l in diff.lines().skip(2) {
        // skip the ---/+++ headers
        if let Some(rest) = l.strip_prefix('+') {
            if !rest.starts_with("++") {
                add += 1;
            }
        } else if let Some(rest) = l.strip_prefix('-') {
            if !rest.starts_with("--") {
                rem += 1;
            }
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
    let mut emitted = 0usize;
    for (i, line) in text.lines().enumerate().skip(offset - 1) {
        if emitted >= limit.min(READ_MAX_LINES) || out.len() > 300_000 {
            out.push_str("\n…(output truncated)");
            break;
        }
        out.push_str(&format!("{:>6}\t{line}\n", i + 1));
        emitted += 1;
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
    match crate::agent::checkpoints::snapshot(
        &ctx.root,
        &format!("write {}", rel_label(&ctx.root, &p)),
    ) {
        Ok(sha) => ctx
            .journal
            .push((sha, format!("write {}", rel_label(&ctx.root, &p)))),
        Err(_) => { /* outside git: proceed without insurance */ }
    }
    if let Err(e) = fs::write(&p, content) {
        return Outcome::err(format!("write failed: {e}"));
    }
    ctx.mark_read(&p);
    let label = rel_label(&ctx.root, &p);
    match prev {
        Some(prev) => {
            let diff = make_diff(&prev, content);
            let (add, rem) = diff_counts(&diff);
            Outcome::ok(format!("wrote {label} (+{add}/-{rem})")).with_diff(diff)
        }
        None => Outcome::ok(format!(
            "created {} ({} lines)",
            label,
            content.lines().count()
        )),
    }
}

fn apply_one(content: &str, old: &str, new: &str, replace_all: bool) -> Result<String, String> {
    if old.is_empty() {
        return err("old_string must not be empty");
    }
    let count = content.matches(old).count();
    if count == 0 {
        return err("old_string not found in file");
    }
    if count > 1 && !replace_all {
        return err(format!(
            "old_string appears {count} times — provide more surrounding context or set replace_all=true"
        ));
    }
    Ok(if replace_all {
        content.replace(old, new)
    } else {
        content.replacen(old, new, 1)
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
    if let Err(e) = fs::write(&p, &updated) {
        return Outcome::err(format!("write failed: {e}"));
    }
    let diff = make_diff(&content, &updated);
    let (add, rem) = diff_counts(&diff);
    Outcome::ok(format!(
        "edited {} (+{add}/-{rem})",
        rel_label(&ctx.root, &p)
    ))
    .with_diff(diff)
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
    let before = content;
    content = staged;
    if let Err(e) = fs::write(&p, &content) {
        return Outcome::err(format!("write failed: {e}"));
    }
    let diff = make_diff(&before, &content);
    let (add, rem) = diff_counts(&diff);
    Outcome::ok(format!(
        "applied {} edit(s) to {} (+{add}/-{rem})",
        edits.len(),
        rel_label(&ctx.root, &p)
    ))
    .with_diff(diff)
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
    let inc = include.map(|g| {
        globset::GlobBuilder::new(g)
            .build()
            .expect("glob")
            .compile_matcher()
    });

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
                out.push_str(&format!("{}:{}: {}\n", shown, i + 1, line.trim_end()));
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
    match crate::agent::checkpoints::snapshot(&ctx.root, &label) {
        Ok(sha) => ctx.journal.push((sha, label)),
        Err(_) => { /* not a git repo: run uninsured like design allows */ }
    }
}

fn rel_label(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .into_owned()
}

// keep json import used even if helpers change
#[allow(unused)]
fn _touch() -> serde_json::Value {
    json!(null)
}
