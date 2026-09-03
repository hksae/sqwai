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

pub fn commit(ctx: &mut ToolCtx, args: &Value) -> Outcome {
    let message = arg(args, "message").trim();
    if message.is_empty() {
        return Outcome::err("git_commit requires a non-empty message");
    }
    if message.len() > 2000 {
        return Outcome::err("git_commit message is too long (maximum 2000 bytes)");
    }
    let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
    if let Ok(sha) = crate::agent::checkpoints::snapshot(&ctx.root, "git_commit") {
        ctx.journal.push((sha, "git_commit".to_string()));
    }
    if all {
        run_git(ctx, &["commit", "-am", message])
    } else {
        run_git(ctx, &["commit", "-m", message])
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
                if let Ok(sha) = crate::agent::checkpoints::snapshot(&ctx.root, "git_branch create")
                {
                    ctx.journal.push((sha, "git_branch create".to_string()));
                }
                run_git(ctx, &["branch", name])
            }
        }
        "switch" => {
            if name.is_empty() {
                Outcome::err("git_branch switch requires a name")
            } else {
                if let Ok(sha) = crate::agent::checkpoints::snapshot(&ctx.root, "git_branch switch")
                {
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

    if let Ok(sha) = crate::agent::checkpoints::snapshot(&ctx.root, "patch") {
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
        Ok(output) if output.status.success() => Outcome::ok("patch applied"),
        Ok(output) => Outcome::err(format!(
            "patch failed: {}",
            truncate(String::from_utf8_lossy(&output.stderr).trim())
        )),
        Err(error) => Outcome::err(format!("patch failed: {error}")),
    }
}
