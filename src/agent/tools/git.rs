//! Dedicated Git and patch tools.
//!
//! Commands are executed without a shell and always rooted at the project
//! directory. This keeps Git arguments separate from shell syntax and lets the
//! existing mutating-tool checkpoint gate protect commits, branches, and patch.

use super::{Outcome, ToolCtx};
use serde_json::Value;
use std::process::{Command, Stdio};

/// output cap in chars: head+tail mid-trim, same budget as exec/webfetch
const MAX_OUTPUT: usize = 30_000;

/// Drain one pipe into the shared buffer until EOF or error.
fn drain_into(stream: impl std::io::Read, buf: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
    use std::io::Read;
    let mut handle = std::io::BufReader::new(stream);
    let mut chunk = [0u8; 8192];
    loop {
        match handle.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buf.lock()
                    .map(|mut b| b.extend_from_slice(&chunk[..n]))
                    .ok();
            }
        }
    }
}

/// Wall-clock bound for one git invocation: hooks and network remotes can
/// hang forever, and a blocking wait wedges the agent (Esc never lands).
/// Generous — hooks legitimately take minutes — but finite.
const GIT_TIMEOUT_SECS: u64 = 300;

/// Wait for an already-spawned git child (stdin written and closed by the
/// caller): drain pipes concurrently so chatty output cannot deadlock the
/// poll loop, kill the whole tree on timeout or Esc.
fn wait_git(
    ctx: &ToolCtx,
    child: &mut std::process::Child,
    what: &str,
    timeout_secs: u64,
) -> Result<std::process::Output, String> {
    use std::sync::{Arc, Mutex};
    let out_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let err_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let mut readers = Vec::new();
    // stdout and stderr have different pipe types: drain each in turn
    if let Some(stream) = child.stdout.take() {
        let buf = Arc::clone(&out_buf);
        readers.push(std::thread::spawn(move || {
            drain_into(stream, &buf);
        }));
    }
    if let Some(stream) = child.stderr.take() {
        let buf = Arc::clone(&err_buf);
        readers.push(std::thread::spawn(move || {
            drain_into(stream, &buf);
        }));
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while readers
                .iter()
                .any(|h: &std::thread::JoinHandle<()>| !h.is_finished())
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let stdout = out_buf.lock().map(|b| b.clone()).unwrap_or_default();
            let stderr = err_buf.lock().map(|b| b.clone()).unwrap_or_default();
            return Ok(std::process::Output {
                status,
                stdout,
                stderr,
            });
        }
        if ctx.cancel_requested() {
            super::exec::kill_tree(child);
            let _ = child.wait();
            return Err("cancelled by user".to_string());
        }
        if std::time::Instant::now() >= deadline {
            super::exec::kill_tree(child);
            let _ = child.wait();
            return Err(format!("git {what} timed out after {timeout_secs}s"));
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn arg<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// Single host-state gate for every git arg that git itself resolves to a
/// path — `rev:path` blobs, diff targets, stage paths. Per-arg, per-tool
/// checks drifted (audit pattern 4); everything user-supplied goes through
/// here.
fn host_owned_path(p: &str) -> bool {
    p.replace('\\', "/").split('/').any(|seg| seg == ".sqwai")
}

/// Git parses a leading `--` in any value position as a flag; ref names and
/// revisions arriving from the model must never start with one (audit H20).
fn run_git(ctx: &ToolCtx, args: &[&str]) -> Outcome {
    let mut child = match Command::new("git")
        .current_dir(&ctx.root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return Outcome::err(format!("git could not start: {error}")),
    };
    let output = match wait_git(
        ctx,
        &mut child,
        args.first().unwrap_or(&"?"),
        GIT_TIMEOUT_SECS,
    ) {
        Ok(output) => output,
        Err(message) => return Outcome::err(message),
    };
    let stdout = super::decode_child_output(&output.stdout);
    let stderr = super::decode_child_output(&output.stderr);
    let text = if stdout.trim().is_empty() {
        stderr.trim().to_string()
    } else if stderr.trim().is_empty() {
        stdout.trim().to_string()
    } else {
        format!("{}\n{}", stdout.trim(), stderr.trim())
    };
    let text = truncate(&text);
    let code = output.status.code();
    if output.status.success() {
        Outcome::ok(if text.is_empty() {
            "ok".to_string()
        } else {
            text
        })
        .with_exit_code(code)
    } else {
        Outcome::err(if text.is_empty() {
            format!("git failed with {}", output.status)
        } else {
            text
        })
        .with_exit_code(code)
    }
}

fn truncate(text: &str) -> String {
    super::trim_middle(text, MAX_OUTPUT)
}

/// Paths from `git status --porcelain=v1` output: the two status columns are
/// dropped, and a rename entry reports its destination (the source is already
/// gone from the worktree). Quoted paths keep the quotes git adds only when the
/// name needs them, so they are stripped here.
pub fn porcelain_paths(status: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in status.lines().filter(|l| l.len() > 3) {
        // `--branch` prefixes the header with "## ", and "!! " marks an
        // ignored entry; neither is a dirty path
        if line.starts_with("##") || line.starts_with("!!") {
            continue;
        }
        let Some(path) = line.get(3..) else { continue };
        let path = path.rsplit(" -> ").next().unwrap_or(path);
        let path = path.trim().trim_matches('"').replace('\\', "/");
        if !path.is_empty() && !out.contains(&path) {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// The worktree's dirty paths, project-relative. Empty (not an error) when the
/// tree is clean or git cannot answer — callers treat it as "nothing to say".
pub fn dirty_paths(ctx: &ToolCtx) -> Vec<String> {
    let outcome = run_git(ctx, &["status", "--porcelain=v1"]);
    if outcome.ok {
        porcelain_paths(&outcome.output)
    } else {
        Vec::new()
    }
}

pub fn status(ctx: &ToolCtx, args: &Value) -> Outcome {
    let porcelain = if args
        .get("porcelain")
        .and_then(Value::as_bool)
        .unwrap_or(true)
    {
        "--porcelain=v1"
    } else {
        "--short"
    };
    run_git(ctx, &["status", porcelain, "--branch"])
}

pub fn diff(ctx: &ToolCtx, args: &Value) -> Outcome {
    let target = arg(args, "target");
    if target.is_empty() {
        run_git(ctx, &["diff", "--"])
    } else if host_owned_path(target) {
        // audit M22: the diff target is a git-resolved path like any other
        Outcome::err(".sqwai is host-owned state and cannot be read with git_diff")
    } else {
        run_git(ctx, &["diff", "--", target])
    }
}

pub fn patch(ctx: &mut ToolCtx, args: &Value) -> Outcome {
    let raw_patch = arg(args, "patch");
    let patch = if raw_patch.trim().is_empty() {
        arg(args, "patchText")
    } else {
        raw_patch
    };
    if patch.trim().is_empty() {
        return Outcome::err("patch requires non-empty diff text");
    }
    if patch.len() > 2_000_000 {
        return Outcome::err("patch is too large (maximum 2 MB)");
    }
    // Inspect files touched by the patch and ensure none escape or touch host-owned state (.sqwai/)
    let touched_files = extract_patch_files(ctx, patch);
    if touched_files.is_empty() {
        return Outcome::err("patch rejected: could not identify any target files in patch");
    }
    let mut resolved_paths = Vec::new();
    for f in &touched_files {
        match ctx.resolve_writable(f) {
            Ok(p) => resolved_paths.push(p),
            Err(e) => return Outcome::err(format!("patch touches forbidden path '{f}': {e}")),
        }
    }

    let is_v4a = is_v4a_patch(patch);
    if is_v4a {
        return apply_structured_patch(ctx, patch, true);
    }

    let mut pre_images: Vec<(std::path::PathBuf, Option<Vec<u8>>)> = Vec::new();
    for p in &resolved_paths {
        let prev = std::fs::read(p).ok();
        pre_images.push((p.clone(), prev));
    }

    let check = Command::new("git")
        .current_dir(&ctx.root)
        .args(["apply", "--check", "--recount", "--whitespace=nowarn", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut check = match check {
        Ok(child) => child,
        Err(_) => return apply_structured_patch(ctx, patch, false),
    };
    if let Some(stdin) = check.stdin.as_mut() {
        use std::io::Write;
        if let Err(error) = stdin.write_all(patch.as_bytes()) {
            return Outcome::err(format!("could not send patch to git: {error}"));
        }
    }
    // stdin written: drop our handle so EOF reaches git, then bounded wait
    drop(check.stdin.take());
    let checked = match wait_git(ctx, &mut check, "apply --check", GIT_TIMEOUT_SECS) {
        Ok(output) => output,
        Err(message) => return Outcome::err(format!("patch check failed: {message}")),
    };
    if !checked.status.success() {
        // Fallback to native 4-tier fuzzy hunk applier when `git apply` fails
        // due to CRLF/LF differences, missing hunk counts (`@@ @@`), or minor
        // whitespace/typographic drift.
        let git_err = super::decode_child_output(&checked.stderr);
        return match apply_structured_patch_inner(ctx, patch, false) {
            Ok(outcome) => outcome,
            Err(_) => Outcome::err(format!("patch rejected: {}", truncate(git_err.trim()))),
        };
    }

    let sha = crate::agent::checkpoints::checkpoint_for_mutation(
        &ctx.root,
        ctx.shadow_store,
        ctx.checkpoint_chain(),
        "patch",
    );
    ctx.checkpoints.push((sha.clone(), "patch".to_string()));
    let checkpoint = Some(sha);

    let mut apply = match Command::new("git")
        .current_dir(&ctx.root)
        .args(["apply", "--recount", "--whitespace=nowarn", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return Outcome::err(format!("patch could not start git: {error}")),
    };
    if let Some(stdin) = apply.stdin.as_mut() {
        use std::io::Write;
        if let Err(error) = stdin.write_all(patch.as_bytes()) {
            return Outcome::err(format!("could not send patch to git: {error}"));
        }
    }
    drop(apply.stdin.take());
    match wait_git(ctx, &mut apply, "apply", GIT_TIMEOUT_SECS) {
        Ok(output) if output.status.success() => {
            let mut file_diffs = Vec::new();
            for (path, before) in pre_images {
                let after = std::fs::read(&path).unwrap_or_default();
                let diff_text = before
                    .as_deref()
                    .map(|b| {
                        let b_str = String::from_utf8_lossy(b);
                        let a_str = String::from_utf8_lossy(&after);
                        super::fs::make_diff(&b_str, &a_str)
                    })
                    .unwrap_or_default();
                let fd = super::fs::file_diff(
                    &path,
                    &ctx.root,
                    before.as_deref(),
                    &after,
                    "patch",
                    checkpoint.clone(),
                    &diff_text,
                );
                file_diffs.push(fd);
                ctx.mark_read(&path);
            }
            Outcome::ok("patch applied")
                .with_diff(patch.to_string())
                .with_file_diffs(file_diffs)
        }
        Ok(output) => Outcome::err(format!(
            "patch failed: {}",
            truncate(super::decode_child_output(&output.stderr).trim())
        )),
        Err(error) => Outcome::err(format!("patch failed: {error}")),
    }
}

fn is_v4a_patch(patch: &str) -> bool {
    patch.lines().any(|l| {
        let t = l.trim();
        t == "*** Begin Patch"
            || t.starts_with("*** Update File: ")
            || t.starts_with("*** Add File: ")
            || t.starts_with("*** Delete File: ")
    })
}

#[derive(Debug)]
struct PatchChunk {
    context_hint: Option<String>,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    eof: bool,
}

#[derive(Debug)]
enum PatchOp {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        chunks: Vec<PatchChunk>,
    },
}

fn parse_v4a_ops(patch: &str) -> Result<Vec<PatchOp>, String> {
    let mut lines: Vec<&str> = patch
        .lines()
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    while lines.first().is_some_and(|l| l.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    let mut idx = 0usize;
    if lines.first().is_some_and(|l| l.trim() == "*** Begin Patch") {
        idx += 1;
    }
    let end = if lines.last().is_some_and(|l| l.trim() == "*** End Patch") {
        lines.len() - 1
    } else {
        lines.len()
    };
    let mut ops = Vec::new();
    while idx < end {
        let line = lines[idx].trim();
        if line.is_empty() || line.starts_with("*** Environment ID:") {
            idx += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix("*** Add File: ") {
            let path = rest.trim().to_string();
            idx += 1;
            let mut body = Vec::new();
            while idx < end && !lines[idx].trim().starts_with("*** ") {
                let raw = lines[idx];
                let content_line = raw.strip_prefix('+').unwrap_or(raw);
                body.push(content_line);
                idx += 1;
            }
            let mut joined = body.join("\n");
            if !joined.is_empty() {
                joined.push('\n');
            }
            ops.push(PatchOp::Add {
                path,
                content: joined,
            });
            continue;
        }
        if let Some(rest) = line.strip_prefix("*** Delete File: ") {
            let path = rest.trim().to_string();
            idx += 1;
            ops.push(PatchOp::Delete { path });
            continue;
        }
        if let Some(rest) = line.strip_prefix("*** Update File: ") {
            let path = rest.trim().to_string();
            idx += 1;
            let mut move_to = None;
            if idx < end
                && let Some(mv) = lines[idx].trim().strip_prefix("*** Move to:")
            {
                let target = mv.trim();
                if target.is_empty() {
                    return Err(format!("Move destination for '{path}' must not be empty"));
                }
                move_to = Some(target.to_string());
                idx += 1;
            }
            let mut chunks: Vec<PatchChunk> = Vec::new();
            while idx < end {
                let cur = lines[idx];
                let trimmed = cur.trim();
                if trimmed.starts_with("*** Add File: ")
                    || trimmed.starts_with("*** Delete File: ")
                    || trimmed.starts_with("*** Update File: ")
                    || trimmed == "*** End Patch"
                {
                    break;
                }
                if trimmed == "*** End of File" {
                    if let Some(last) = chunks.last_mut() {
                        last.eof = true;
                    }
                    idx += 1;
                    continue;
                }
                if trimmed == "@@" || trimmed.starts_with("@@ ") {
                    let hint = trimmed
                        .strip_prefix("@@")
                        .map(|s| s.trim_start_matches('@').trim())
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                    chunks.push(PatchChunk {
                        context_hint: hint,
                        old_lines: Vec::new(),
                        new_lines: Vec::new(),
                        eof: false,
                    });
                    idx += 1;
                    continue;
                }
                if chunks.is_empty() {
                    chunks.push(PatchChunk {
                        context_hint: None,
                        old_lines: Vec::new(),
                        new_lines: Vec::new(),
                        eof: false,
                    });
                }
                let chunk = chunks.last_mut().unwrap();
                if cur.is_empty() {
                    chunk.old_lines.push(String::new());
                    chunk.new_lines.push(String::new());
                } else if let Some(s) = cur.strip_prefix(' ') {
                    chunk.old_lines.push(s.to_string());
                    chunk.new_lines.push(s.to_string());
                } else if let Some(s) = cur.strip_prefix('-') {
                    chunk.old_lines.push(s.to_string());
                } else if let Some(s) = cur.strip_prefix('+') {
                    chunk.new_lines.push(s.to_string());
                } else {
                    // Models frequently omit the leading space on context lines in V4A patches
                    chunk.old_lines.push(cur.to_string());
                    chunk.new_lines.push(cur.to_string());
                }
                idx += 1;
            }
            if chunks.is_empty() {
                return Err(format!("update hunk for '{path}' is empty"));
            }
            ops.push(PatchOp::Update {
                path,
                move_to,
                chunks,
            });
            continue;
        }
        return Err(format!("unrecognized patch header: {line}"));
    }
    Ok(ops)
}

fn strip_diff_prefix(raw: &str) -> Option<String> {
    let t = raw.split('\t').next().unwrap_or(raw).trim();
    if t == "/dev/null" || t.is_empty() {
        return None;
    }
    let clean = t
        .strip_prefix("a/")
        .or_else(|| t.strip_prefix("b/"))
        .unwrap_or(t);
    (!clean.is_empty()).then(|| clean.to_string())
}

fn parse_unified_ops(patch: &str) -> Result<Vec<PatchOp>, String> {
    let lines: Vec<&str> = patch
        .lines()
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let mut idx = 0usize;
    let mut ops = Vec::new();
    while idx < lines.len() {
        let line = lines[idx];
        if !line.starts_with("--- ") {
            idx += 1;
            continue;
        }
        let old_file = strip_diff_prefix(&line[4..]);
        idx += 1;
        if idx >= lines.len() || !lines[idx].starts_with("+++ ") {
            return Err("missing '+++' header after '---'".into());
        }
        let new_file = strip_diff_prefix(&lines[idx][4..]);
        idx += 1;

        let mut chunks: Vec<PatchChunk> = Vec::new();
        while idx < lines.len() {
            let cur = lines[idx];
            if cur.starts_with("diff ") || cur.starts_with("--- ") || cur.starts_with("Index: ") {
                break;
            }
            if cur.starts_with("@@") {
                // Extract optional trailing function context after second `@@`
                let hint = cur
                    .strip_prefix("@@")
                    .and_then(|s| s.split_once("@@"))
                    .map(|(_, tail)| tail.trim())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                chunks.push(PatchChunk {
                    context_hint: hint,
                    old_lines: Vec::new(),
                    new_lines: Vec::new(),
                    eof: false,
                });
                idx += 1;
                continue;
            }
            if cur.starts_with("\\ No newline at end of file") {
                idx += 1;
                continue;
            }
            if let Some(chunk) = chunks.last_mut() {
                if cur.is_empty() {
                    chunk.old_lines.push(String::new());
                    chunk.new_lines.push(String::new());
                } else if let Some(s) = cur.strip_prefix(' ') {
                    chunk.old_lines.push(s.to_string());
                    chunk.new_lines.push(s.to_string());
                } else if let Some(s) = cur.strip_prefix('-') {
                    chunk.old_lines.push(s.to_string());
                } else if let Some(s) = cur.strip_prefix('+') {
                    chunk.new_lines.push(s.to_string());
                }
            }
            idx += 1;
        }

        match (old_file, new_file) {
            (None, Some(path)) => {
                let mut added: Vec<String> = Vec::new();
                for c in chunks {
                    added.extend(c.new_lines);
                }
                let mut content = added.join("\n");
                if !content.is_empty() {
                    content.push('\n');
                }
                ops.push(PatchOp::Add { path, content });
            }
            (Some(path), None) => {
                ops.push(PatchOp::Delete { path });
            }
            (Some(old_p), Some(new_p)) => {
                let move_to = (old_p != new_p).then_some(new_p);
                ops.push(PatchOp::Update {
                    path: old_p,
                    move_to,
                    chunks,
                });
            }
            (None, None) => {}
        }
    }
    if ops.is_empty() {
        return Err("no valid unified diff hunks found".into());
    }
    Ok(ops)
}

/// 4-tier progressive sequence seeker (OpenCode `seek` convention):
/// 1. Exact line equality
/// 2. Right-trimmed equality (`trim_end`)
/// 3. Both-ends trimmed equality (`trim`)
/// 4. Typographic-normalized trimmed equality (`normalize_typographic`)
fn seek_lines(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() || pattern.len() > lines.len() || start > lines.len() - pattern.len() {
        return None;
    }
    let tiers: [fn(&str, &str) -> bool; 4] = [
        |a, b| a == b,
        |a, b| a.trim_end() == b.trim_end(),
        |a, b| a.trim() == b.trim(),
        |a, b| {
            super::fs::normalize_typographic(a.trim()) == super::fs::normalize_typographic(b.trim())
        },
    ];
    if eof {
        let offset = lines.len() - pattern.len();
        if offset < start {
            return None;
        }
        for cmp in tiers {
            if (0..pattern.len()).all(|k| cmp(&lines[offset + k], &pattern[k])) {
                return Some(offset);
            }
        }
        return None;
    }
    for cmp in tiers {
        for offset in start..=(lines.len() - pattern.len()) {
            if (0..pattern.len()).all(|k| cmp(&lines[offset + k], &pattern[k])) {
                return Some(offset);
            }
        }
    }
    None
}

fn apply_chunks_to_text(
    path: &str,
    original: &str,
    chunks: &[PatchChunk],
) -> Result<String, String> {
    let eol = if original.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let had_trailing_nl = original.ends_with('\n') || original.is_empty();
    let mut lines: Vec<String> = original
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
        .collect();
    if original.ends_with('\n') {
        lines.pop();
    }

    let mut replacements: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut cursor = 0usize;
    for chunk in chunks {
        if let Some(ctx_hint) = &chunk.context_hint
            && let Some(pos) = seek_lines(&lines, std::slice::from_ref(ctx_hint), cursor, false)
        {
            cursor = pos;
        }
        if chunk.old_lines.is_empty() {
            replacements.push((lines.len(), 0, chunk.new_lines.clone()));
            continue;
        }
        let mut old_slice = chunk.old_lines.as_slice();
        let mut new_slice = chunk.new_lines.as_slice();
        let mut found = seek_lines(&lines, old_slice, cursor, chunk.eof)
            .or_else(|| seek_lines(&lines, old_slice, 0, chunk.eof));
        if found.is_none() && old_slice.last().is_some_and(|l| l.is_empty()) {
            old_slice = &old_slice[..old_slice.len() - 1];
            if new_slice.last().is_some_and(|l| l.is_empty()) {
                new_slice = &new_slice[..new_slice.len() - 1];
            }
            found = seek_lines(&lines, old_slice, cursor, chunk.eof)
                .or_else(|| seek_lines(&lines, old_slice, 0, chunk.eof));
        }
        let Some(pos) = found else {
            return Err(format!(
                "failed to find hunk lines in {path}:\n{}",
                chunk.old_lines.join("\n")
            ));
        };
        replacements.push((pos, old_slice.len(), new_slice.to_vec()));
        cursor = pos + old_slice.len();
    }

    replacements.sort_by_key(|(start, _, _)| *start);
    for (start, remove_len, insert_lines) in replacements.into_iter().rev() {
        lines.splice(start..start + remove_len, insert_lines);
    }
    let mut out = lines.join(eol);
    if had_trailing_nl && !out.is_empty() {
        out.push_str(eol);
    }
    Ok(out)
}

fn apply_structured_patch(ctx: &mut ToolCtx, patch: &str, v4a: bool) -> Outcome {
    match apply_structured_patch_inner(ctx, patch, v4a) {
        Ok(outcome) => outcome,
        Err(err_msg) => Outcome::err(format!("patch rejected: {err_msg}")),
    }
}

fn apply_structured_patch_inner(
    ctx: &mut ToolCtx,
    patch: &str,
    v4a: bool,
) -> Result<Outcome, String> {
    let ops = if v4a {
        parse_v4a_ops(patch)?
    } else {
        parse_unified_ops(patch)?
    };
    struct StagedWrite {
        src_path: std::path::PathBuf,
        dst_path: std::path::PathBuf,
        before: Option<Vec<u8>>,
        after: Option<Vec<u8>>,
    }
    let mut staged: Vec<StagedWrite> = Vec::new();
    for op in ops {
        match op {
            PatchOp::Add { path, content } => {
                let resolved = ctx
                    .resolve_writable(&path)
                    .map_err(|e| format!("forbidden path '{path}': {e}"))?;
                let prev = std::fs::read(&resolved).ok();
                staged.push(StagedWrite {
                    src_path: resolved.clone(),
                    dst_path: resolved,
                    before: prev,
                    after: Some(content.into_bytes()),
                });
            }
            PatchOp::Delete { path } => {
                let resolved = ctx
                    .resolve_writable(&path)
                    .map_err(|e| format!("forbidden path '{path}': {e}"))?;
                let prev =
                    std::fs::read(&resolved).map_err(|e| format!("cannot delete '{path}': {e}"))?;
                staged.push(StagedWrite {
                    src_path: resolved.clone(),
                    dst_path: resolved,
                    before: Some(prev),
                    after: None,
                });
            }
            PatchOp::Update {
                path,
                move_to,
                chunks,
            } => {
                let src_resolved = ctx
                    .resolve_writable(&path)
                    .map_err(|e| format!("forbidden path '{path}': {e}"))?;
                let dst_resolved = match &move_to {
                    Some(mv) => ctx
                        .resolve_writable(mv)
                        .map_err(|e| format!("forbidden path '{mv}': {e}"))?,
                    None => src_resolved.clone(),
                };
                // Support multiple update blocks targeting the same file in one patch
                let current_bytes = if let Some(prev_staged) = staged
                    .iter()
                    .rev()
                    .find(|s| s.dst_path == src_resolved)
                    .and_then(|s| s.after.clone())
                {
                    prev_staged
                } else {
                    std::fs::read(&src_resolved)
                        .map_err(|e| format!("cannot read '{path}': {e}"))?
                };
                let current_text = String::from_utf8_lossy(&current_bytes).into_owned();
                let has_bom = current_text.starts_with('\u{feff}');
                let body = current_text
                    .strip_prefix('\u{feff}')
                    .unwrap_or(&current_text);
                let updated_body = apply_chunks_to_text(&path, body, &chunks)?;
                let final_text = if has_bom {
                    format!("\u{feff}{updated_body}")
                } else {
                    updated_body
                };
                if let Some(existing) = staged.iter_mut().rev().find(|s| s.dst_path == src_resolved)
                {
                    existing.dst_path = dst_resolved;
                    existing.after = Some(final_text.into_bytes());
                } else {
                    staged.push(StagedWrite {
                        src_path: src_resolved,
                        dst_path: dst_resolved,
                        before: Some(current_bytes),
                        after: Some(final_text.into_bytes()),
                    });
                }
            }
        }
    }

    let sha = crate::agent::checkpoints::checkpoint_for_mutation(
        &ctx.root,
        ctx.shadow_store,
        ctx.checkpoint_chain(),
        "patch",
    );
    ctx.checkpoints.push((sha.clone(), "patch".to_string()));
    let checkpoint = Some(sha);

    let mut file_diffs = Vec::new();
    for item in staged {
        match &item.after {
            Some(bytes) => {
                if let Some(parent) = item.dst_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(&item.dst_path, bytes)
                    .map_err(|e| format!("failed to write {}: {e}", item.dst_path.display()))?;
                if item.src_path != item.dst_path && item.src_path.exists() {
                    let _ = std::fs::remove_file(&item.src_path);
                }
                let b_str = item
                    .before
                    .as_deref()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .unwrap_or_default();
                let a_str = String::from_utf8_lossy(bytes);
                let diff_text = super::fs::make_diff(&b_str, &a_str);
                let fd = super::fs::file_diff(
                    &item.dst_path,
                    &ctx.root,
                    item.before.as_deref(),
                    bytes,
                    "patch",
                    checkpoint.clone(),
                    &diff_text,
                );
                file_diffs.push(fd);
                ctx.mark_read(&item.dst_path);
            }
            None => {
                let _ = std::fs::remove_file(&item.src_path);
                let b_str = item
                    .before
                    .as_deref()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .unwrap_or_default();
                let diff_text = super::fs::make_diff(&b_str, "");
                let fd = super::fs::file_diff(
                    &item.src_path,
                    &ctx.root,
                    item.before.as_deref(),
                    b"",
                    "patch",
                    checkpoint.clone(),
                    &diff_text,
                );
                file_diffs.push(fd);
            }
        }
    }

    Ok(Outcome::ok("patch applied")
        .with_diff(patch.to_string())
        .with_file_diffs(file_diffs))
}

pub(super) fn extract_patch_files(ctx: &ToolCtx, patch: &str) -> Vec<String> {
    fn push_unique(files: &mut Vec<String>, p: String) {
        if !p.is_empty() && !files.contains(&p) {
            files.push(p);
        }
    }
    let mut files: Vec<String> = Vec::new();

    for raw_line in patch.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let trimmed = line.trim();
        for prefix in [
            "*** Add File: ",
            "*** Delete File: ",
            "*** Update File: ",
            "*** Move to: ",
        ] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                push_unique(&mut files, rest.trim().to_string());
            }
        }
        if let Some(rest) = line
            .strip_prefix("--- ")
            .or_else(|| line.strip_prefix("+++ "))
            && let Some(clean) = strip_diff_prefix(rest)
        {
            push_unique(&mut files, clean);
        }
    }

    if files.is_empty() {
        use std::io::Write;
        if let Ok(mut cmd) = Command::new("git")
            .current_dir(&ctx.root)
            .args(["apply", "--numstat", "--recount", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(stdin) = cmd.stdin.as_mut() {
                let _ = stdin.write_all(patch.as_bytes());
            }
            drop(cmd.stdin.take());
            if let Ok(o) = wait_git(ctx, &mut cmd, "apply --numstat", GIT_TIMEOUT_SECS)
                && o.status.success()
            {
                let stdout = String::from_utf8_lossy(&output_bytes(&o.stdout));
                for line in stdout.lines() {
                    let parts: Vec<&str> = line.split('\t').collect();
                    if parts.len() >= 3 {
                        push_unique(&mut files, parts[2].to_string());
                    }
                }
            }
        }
    }
    files
}

fn output_bytes(b: &[u8]) -> &[u8] {
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn patch_rejects_modifying_host_owned_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolCtx::new(dir.path());
        let forbidden_patch =
            "--- a/.sqwai/plan.json\n+++ b/.sqwai/plan.json\n@@ -1 +1 @@\n-old\n+new\n";
        let outcome = patch(&mut ctx, &json!({"patch": forbidden_patch}));
        assert!(!outcome.ok);
        assert!(outcome.output.contains("forbidden path"));
    }

    /// A hung git child (sleep-like stand-in: hooks and network remotes hang
    /// the same way) must time out instead of wedging the agent, and Esc
    /// must cancel it mid-wait — both kill the tree first.
    #[test]
    fn wait_git_times_out_and_honors_cancel() {
        use std::sync::{Arc, atomic::AtomicBool};
        // long sleeper, cross-platform: ping with a high count ends only
        // when killed (mirrors the exec background-job tests)
        #[cfg(windows)]
        fn sleeper() -> std::process::Command {
            let mut cmd = std::process::Command::new("ping");
            cmd.args(["-n", "300", "127.0.0.1"]);
            cmd
        }
        #[cfg(not(windows))]
        fn sleeper() -> std::process::Command {
            let mut cmd = std::process::Command::new("sleep");
            cmd.arg("300");
            cmd
        }
        let dir = tempfile::tempdir().unwrap();
        // timeout: 1s bound on a 300s sleeper
        let ctx = ToolCtx::new(dir.path());
        let mut child = sleeper()
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("sleeper must spawn");
        let err = wait_git(&ctx, &mut child, "test", 1).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(
            child.try_wait().ok().flatten().is_some(),
            "timed-out child must be reaped dead, not left running"
        );
        // cancel: pre-armed flag aborts the wait immediately
        let flag = Arc::new(AtomicBool::new(true));
        let ctx = ToolCtx::new(dir.path()).with_cancel(flag);
        let mut child = sleeper()
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("sleeper must spawn");
        let err = wait_git(&ctx, &mut child, "test", 300).unwrap_err();
        assert!(err.contains("cancelled"), "{err}");
        assert!(
            child.try_wait().ok().flatten().is_some(),
            "cancelled child must be reaped dead, not left running"
        );
    }

    #[test]
    fn patch_stores_layer1_blobs_and_records_file_diff() {
        let dir = tempfile::tempdir().unwrap();
        Command::new("git")
            .current_dir(dir.path())
            .args(["init"])
            .output()
            .unwrap();
        Command::new("git")
            .current_dir(dir.path())
            .args(["config", "user.name", "sqwai-test"])
            .output()
            .unwrap();
        Command::new("git")
            .current_dir(dir.path())
            .args(["config", "user.email", "test@test.local"])
            .output()
            .unwrap();
        Command::new("git")
            .current_dir(dir.path())
            .args(["config", "core.autocrlf", "false"])
            .output()
            .unwrap();

        let file = dir.path().join("hello.txt");
        std::fs::write(&file, "line1\nline2\n").unwrap();
        Command::new("git")
            .current_dir(dir.path())
            .args(["add", "hello.txt"])
            .output()
            .unwrap();
        Command::new("git")
            .current_dir(dir.path())
            .args(["commit", "-m", "init"])
            .output()
            .unwrap();

        let mut ctx = ToolCtx::new(dir.path());
        let unified_patch =
            "--- a/hello.txt\n+++ b/hello.txt\n@@ -1,2 +1,2 @@\n line1\n-line2\n+line2_modified\n";
        let outcome = patch(&mut ctx, &json!({"patch": unified_patch}));
        assert!(outcome.ok, "patch failed: {}", outcome.output);
        assert!(outcome.file_diff.is_some());
        let fd = outcome.file_diff.as_ref().unwrap();
        assert_eq!(fd.path, "hello.txt");
        assert_eq!(fd.added, 1);
        assert_eq!(fd.removed, 1);
        assert!(fd.blob_before.is_some());
        assert!(fd.blob_after.is_some());

        // Verify blob_before in blob store
        let blob_hash = fd.blob_before.as_ref().unwrap();
        let blob_bytes = crate::agent::blobs::get(dir.path(), blob_hash).unwrap();
        assert_eq!(blob_bytes, b"line1\nline2\n");
    }

    fn init_repo(dir: &std::path::Path) {
        for args in [
            &["init"][..],
            &["config", "user.name", "sqwai-test"][..],
            &["config", "user.email", "test@test.local"][..],
            &["config", "core.autocrlf", "false"][..],
        ] {
            Command::new("git")
                .current_dir(dir)
                .args(args)
                .output()
                .unwrap();
        }
    }

    fn git_out(dir: &std::path::Path, args: &[&str]) -> String {
        String::from_utf8(
            Command::new("git")
                .current_dir(dir)
                .args(args)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
    }

    /// Audit C4/M22: `git show <rev>:<path>` and `git diff -- <path>` reach
    /// tracked blobs directly — the `.sqwai` segment check must cover the
    /// commit arg and the diff target, not just the `path` arg. The user
    /// committed `.sqwai/` themselves here (no gitignore writer by design);
    /// the tools must still refuse to read host state back.
    #[test]
    fn git_diff_refuses_to_leak_host_owned_state() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        std::fs::create_dir_all(dir.path().join(".sqwai/events")).unwrap();
        std::fs::write(
            dir.path().join(".sqwai/events/active.json"),
            "TOP-SECRET-PLAN",
        )
        .unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "open\n").unwrap();
        git_out(dir.path(), &["add", "-A"]);
        git_out(dir.path(), &["commit", "-m", "init"]);
        std::fs::write(
            dir.path().join(".sqwai/events/active.json"),
            "TOP-SECRET-PLAN-v2",
        )
        .unwrap();

        let ctx = ToolCtx::new(dir.path());
        let d = diff(&ctx, &json!({"target": ".sqwai/events/active.json"}));
        assert!(
            !d.output.contains("TOP-SECRET-PLAN"),
            "git_diff leaked host state: {}",
            d.output
        );
    }

    #[test]
    fn patch_supports_v4a_envelope_and_fuzzy_unified_diffs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("app.rs"),
            "fn alpha() {\n    let x = 1;\n}\n\nfn beta() {\n    let y = 2;\n}\n",
        )
        .unwrap();
        let mut ctx = ToolCtx::new(dir.path());

        // 1. V4A envelope (`*** Begin Patch` / `*** Update File:` / `*** Add File:`)
        let v4a = "*** Begin Patch\n\
                   *** Update File: app.rs\n\
                   @@ fn beta()\n\
                    fn beta() {\n\
                   -    let y = 2;\n\
                   +    let y = 20;\n\
                    }\n\
                   *** Add File: added.txt\n\
                   +hello v4a\n\
                   *** End Patch\n";
        let out = patch(&mut ctx, &json!({"patch": v4a}));
        assert!(out.ok, "v4a patch failed: {}", out.output);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("app.rs")).unwrap(),
            "fn alpha() {\n    let x = 1;\n}\n\nfn beta() {\n    let y = 20;\n}\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("added.txt")).unwrap(),
            "hello v4a\n"
        );

        // 2. Unified diff with wrong line numbers in `@@ -99,5 +99,5 @@` still applies cleanly
        let bad_counts_diff = "--- a/app.rs\n\
                               +++ b/app.rs\n\
                               @@ -99,9 +99,9 @@\n\
                                fn alpha() {\n\
                               -    let x = 1;\n\
                               +    let x = 10;\n\
                                }\n";
        let out2 = patch(&mut ctx, &json!({"patch": bad_counts_diff}));
        assert!(out2.ok, "fuzzy unified diff failed: {}", out2.output);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("app.rs")).unwrap(),
            "fn alpha() {\n    let x = 10;\n}\n\nfn beta() {\n    let y = 20;\n}\n"
        );
    }
}
