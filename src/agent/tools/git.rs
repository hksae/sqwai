//! Dedicated Git and patch tools.
//!
//! Commands are executed without a shell and always rooted at the project
//! directory. This keeps Git arguments separate from shell syntax and lets the
//! existing mutating-tool checkpoint gate protect commits, branches, and patch.

use super::{Outcome, ToolCtx};
use serde_json::Value;
use std::process::{Command, Stdio};

const MAX_OUTPUT: usize = 40_000;

fn arg<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn run_git(ctx: &ToolCtx, args: &[&str]) -> Outcome {
    let output = Command::new("git")
        .current_dir(&ctx.root)
        .args(args)
        .stdin(Stdio::null())
        .output();
    let output = match output {
        Ok(output) => output,
        Err(error) => return Outcome::err(format!("git could not start: {error}")),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = if stdout.trim().is_empty() {
        stderr.trim().to_string()
    } else if stderr.trim().is_empty() {
        stdout.trim().to_string()
    } else {
        format!("{}\n{}", stdout.trim(), stderr.trim())
    };
    let text = truncate(&text);
    if output.status.success() {
        Outcome::ok(if text.is_empty() {
            "ok".to_string()
        } else {
            text
        })
    } else {
        Outcome::err(if text.is_empty() {
            format!("git failed with {}", output.status)
        } else {
            text
        })
    }
}

fn truncate(text: &str) -> String {
    if text.len() <= MAX_OUTPUT {
        return text.to_string();
    }
    let mut start = text.len() - MAX_OUTPUT;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[output truncated]\n{}", &text[start..])
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
    } else {
        run_git(ctx, &["diff", "--", target])
    }
}

pub fn log(ctx: &ToolCtx, args: &Value) -> Outcome {
    let count = args
        .get("count")
        .and_then(Value::as_u64)
        .unwrap_or(10)
        .clamp(1, 100);
    let format = arg(args, "format");
    let format = if format.is_empty() {
        "%h %s (%an, %ad)"
    } else {
        format
    };
    let count_arg = format!("-{count}");
    run_git(
        ctx,
        &[
            "log",
            &count_arg,
            "--date=short",
            &format!("--format={format}"),
        ],
    )
}

/// `git_show`: what a commit changed, or what a file looked like at a commit.
/// Read-only mirror of `git show` for the agent: a commit shows the message,
/// the diff stat and the patch; a `path` shows that file's content at the
/// revision (`git show <rev>:<path>`).
pub fn show(ctx: &ToolCtx, args: &Value) -> Outcome {
    let commit = arg(args, "commit");
    let commit = if commit.trim().is_empty() {
        "HEAD"
    } else {
        commit.trim()
    };
    let path = arg(args, "path").trim();
    if path.is_empty() {
        return run_git(ctx, &["show", commit, "--stat=200", "--patch"]);
    }
    // git paths are forward-slashed relative to the repo root; host-owned
    // state is not readable through other tools and not through this one
    let path = path.replace('\\', "/");
    if path.split('/').any(|seg| seg == ".sqwai") {
        return Outcome::err(".sqwai is host-owned state and cannot be read with git_show");
    }
    if path.starts_with('/') || path.contains("..") {
        return Outcome::err(format!("bad path '{path}': use a repo-relative path"));
    }
    run_git(ctx, &["show", &format!("{commit}:{path}")])
}

pub fn commit(ctx: &mut ToolCtx, args: &Value) -> Outcome {
    let message = arg(args, "message").trim();
    if message.is_empty() {
        return Outcome::err("git_commit requires a non-empty message");
    }
    if message.len() > 2000 {
        return Outcome::err("git_commit message is too long (maximum 2000 bytes)");
    }
    let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
    if let Ok(Some(sha)) = crate::agent::checkpoints::snapshot_session(
        &ctx.root,
        ctx.shadow_store,
        &ctx.session_id,
        "git_commit",
    ) {
        ctx.journal.push((sha, "git_commit".to_string()));
    }
    if all {
        run_git(ctx, &["commit", "-am", message])
    } else {
        run_git(ctx, &["commit", "-m", message])
    }
}

pub fn stage(ctx: &ToolCtx, args: &Value) -> Outcome {
    let action = arg(args, "action");
    let action = if action.trim().is_empty() {
        "add"
    } else {
        action.trim()
    };
    if !matches!(action, "add" | "reset") {
        return Outcome::err("git_stage action must be add or reset");
    }

    let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);

    let mut raw_paths = Vec::new();
    if let Some(arr) = args.get("paths").and_then(Value::as_array) {
        for v in arr {
            if let Some(s) = v.as_str() {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    raw_paths.push(trimmed.to_string());
                }
            }
        }
    } else if let Some(s) = args.get("paths").and_then(Value::as_str) {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            raw_paths.push(trimmed.to_string());
        }
    }
    if let Some(s) = args.get("path").and_then(Value::as_str) {
        let trimmed = s.trim();
        if !trimmed.is_empty() && !raw_paths.contains(&trimmed.to_string()) {
            raw_paths.push(trimmed.to_string());
        }
    }

    if !all && raw_paths.is_empty() {
        return Outcome::err("git_stage requires either all: true or non-empty paths");
    }

    if all {
        if action == "add" {
            run_git(ctx, &["add", "-A"])
        } else {
            run_git(ctx, &["reset"])
        }
    } else {
        let mut clean_paths = Vec::new();
        for p in &raw_paths {
            let p_norm = p.replace('\\', "/");
            if p_norm.split('/').any(|seg| seg == ".sqwai") {
                return Outcome::err(format!("cannot stage host-owned state in path '{p}'"));
            }
            if p_norm.starts_with('/') || p_norm.contains("..") {
                return Outcome::err(format!("bad path '{p}': use a repo-relative path"));
            }
            match ctx.resolve(p) {
                Ok(_) => clean_paths.push(p_norm),
                Err(e) => return Outcome::err(format!("forbidden path '{p}': {e}")),
            }
        }

        let mut git_args: Vec<&str> = Vec::with_capacity(2 + clean_paths.len());
        if action == "add" {
            git_args.push("add");
        } else {
            git_args.push("reset");
        }
        git_args.push("--");
        for p in &clean_paths {
            git_args.push(p.as_str());
        }
        run_git(ctx, &git_args)
    }
}

pub fn branch(ctx: &mut ToolCtx, args: &Value) -> Outcome {
    let action = arg(args, "action");
    let name = arg(args, "name").trim();
    match action {
        "list" | "" => run_git(ctx, &["branch", "--list"]),
        "current" => run_git(ctx, &["branch", "--show-current"]),
        "create" => {
            if name.is_empty() {
                Outcome::err("git_branch create requires a name")
            } else {
                if let Ok(Some(sha)) = crate::agent::checkpoints::snapshot_session(
                    &ctx.root,
                    ctx.shadow_store,
                    &ctx.session_id,
                    "git_branch create",
                ) {
                    ctx.journal.push((sha, "git_branch create".to_string()));
                }
                run_git(ctx, &["branch", name])
            }
        }
        "switch" => {
            if name.is_empty() {
                Outcome::err("git_branch switch requires a name")
            } else {
                if let Ok(Some(sha)) = crate::agent::checkpoints::snapshot_session(
                    &ctx.root,
                    ctx.shadow_store,
                    &ctx.session_id,
                    "git_branch switch",
                ) {
                    ctx.journal.push((sha, "git_branch switch".to_string()));
                }
                run_git(ctx, &["switch", name])
            }
        }
        _ => Outcome::err("git_branch action must be list, current, create, or switch"),
    }
}

pub fn patch(ctx: &mut ToolCtx, args: &Value) -> Outcome {
    let patch = arg(args, "patch");
    if patch.trim().is_empty() {
        return Outcome::err("patch requires non-empty unified diff text");
    }
    if patch.len() > 2_000_000 {
        return Outcome::err("patch is too large (maximum 2 MB)");
    }
    // Inspect files touched by the patch and ensure none escape or touch host-owned state (.sqwai/)
    let touched_files = extract_patch_files(&ctx.root, patch);
    let mut resolved_paths = Vec::new();
    for f in &touched_files {
        match ctx.resolve(f) {
            Ok(p) => resolved_paths.push(p),
            Err(e) => return Outcome::err(format!("patch touches forbidden path '{f}': {e}")),
        }
    }

    let check = Command::new("git")
        .current_dir(&ctx.root)
        .args(["apply", "--check", "--whitespace=error", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut check = match check {
        Ok(child) => child,
        Err(error) => return Outcome::err(format!("patch could not start git: {error}")),
    };
    if let Some(stdin) = check.stdin.as_mut() {
        use std::io::Write;
        if let Err(error) = stdin.write_all(patch.as_bytes()) {
            return Outcome::err(format!("could not send patch to git: {error}"));
        }
    }
    let checked = match check.wait_with_output() {
        Ok(output) => output,
        Err(error) => return Outcome::err(format!("patch check failed: {error}")),
    };
    if !checked.status.success() {
        let error = String::from_utf8_lossy(&checked.stderr);
        return Outcome::err(format!("patch rejected: {}", truncate(error.trim())));
    }

    if let Ok(Some(sha)) = crate::agent::checkpoints::snapshot_session(
        &ctx.root,
        ctx.shadow_store,
        &ctx.session_id,
        "patch",
    ) {
        ctx.journal.push((sha, "patch".to_string()));
    }

    let mut apply = match Command::new("git")
        .current_dir(&ctx.root)
        .args(["apply", "--whitespace=error", "-"])
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
    match apply.wait_with_output() {
        Ok(output) if output.status.success() => {
            // Keep ToolCtx read_state in sync so subsequent edits do not fail as stale
            for path in &resolved_paths {
                ctx.mark_read(path);
            }
            Outcome::ok("patch applied")
        }
        Ok(output) => Outcome::err(format!(
            "patch failed: {}",
            truncate(String::from_utf8_lossy(&output.stderr).trim())
        )),
        Err(error) => Outcome::err(format!("patch failed: {error}")),
    }
}

fn extract_patch_files(root: &std::path::Path, patch: &str) -> Vec<String> {
    use std::io::Write;
    let mut cmd = match Command::new("git")
        .current_dir(root)
        .args(["apply", "--numstat", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    if let Some(stdin) = cmd.stdin.as_mut() {
        let _ = stdin.write_all(patch.as_bytes());
    }
    let output = match cmd.wait_with_output() {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() >= 3 {
                Some(parts[2].to_string())
            } else {
                None
            }
        })
        .collect()
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
}
