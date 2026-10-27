//! Dedicated Git tools: the worktree status and diff views.
//!
//! Commands are executed without a shell and always rooted at the project
//! directory. This keeps Git arguments separate from shell syntax and lets the
//! existing mutating-tool checkpoint gate protect them.

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
    let outcome = git_text(ctx, args);
    if outcome.ok && outcome.output.is_empty() {
        // A command that succeeded with nothing to say. Callers that report to
        // the model must not stop here: for a diff or a status, silence carries
        // information, and `ok` reads like confirmation of something else.
        return Outcome::ok("ok".to_string()).with_exit_code(outcome.exit_code);
    }
    outcome
}

/// The same call with git's own bytes, emptiness left empty.
fn git_text(ctx: &ToolCtx, args: &[&str]) -> Outcome {
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
        Outcome::ok(text).with_exit_code(code)
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
    let outcome = git_text(ctx, &["status", porcelain, "--branch"]);
    if !outcome.ok || !porcelain_paths(&outcome.output).is_empty() {
        return outcome;
    }
    // `--branch` always prints a header, so a quiet tree arrives as 36 bytes of
    // branch line and nothing else. The model cannot tell that from "my files
    // are hidden here", and re-asks; say which of the two it is.
    let note = "no tracked changes and no untracked files; gitignored paths never appear in this list";
    Outcome::ok(if outcome.output.trim().is_empty() {
        note.to_string()
    } else {
        format!("{}\n{note}", outcome.output.trim())
    })
    .with_exit_code(outcome.exit_code)
}

pub fn diff(ctx: &ToolCtx, args: &Value) -> Outcome {
    let target = arg(args, "target");
    if !target.is_empty() && host_owned_path(target) {
        // audit M22: the diff target is a git-resolved path like any other
        return Outcome::err(".sqwai is host-owned state and cannot be read with git_diff");
    }
    let call = if target.is_empty() {
        ["diff", "--"].to_vec()
    } else {
        ["diff", "--", target].to_vec()
    };
    let outcome = git_text(ctx, &call);
    if !outcome.ok || !outcome.output.trim().is_empty() {
        return outcome;
    }
    // An empty diff is the answer git gives for three different situations, and
    // `ok` claims none of them. Name the one that applies.
    let why = if target.is_empty() {
        "no unstaged changes in tracked files; untracked and gitignored paths have no diff to show".to_string()
    } else {
        path_state(ctx, target)
    };
    Outcome::ok(why).with_exit_code(outcome.exit_code)
}

/// What git makes of one path when a diff of it came back empty.
fn path_state(ctx: &ToolCtx, target: &str) -> String {
    let probe = git_text(
        ctx,
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignored",
            "--",
            target,
        ],
    );
    if !probe.ok {
        return format!("no diff for {target}, and git could not say why: {}", probe.output.trim());
    }
    let line = probe.output.trim().to_string();
    if let Some(rest) = line.strip_prefix("!! ") {
        return format!("{rest} is gitignored: git holds no version of it, so there is nothing to diff — `read` shows the file");
    }
    if let Some(rest) = line.strip_prefix("?? ") {
        return format!("{rest} is untracked: git has no stored version to diff against — `read` shows the file");
    }
    if line.is_empty() {
        let tracked = git_text(ctx, &["ls-files", "--error-unmatch", "--", target]);
        return if tracked.ok {
            format!("{target} is tracked and identical to its committed version: no unstaged change")
        } else {
            format!("git has no entry for {target}: not tracked, not untracked, not ignored — check the path")
        };
    }
    format!("git reports `{line}` for {target}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    /// An empty diff is git's answer for three different situations, and `ok`
    /// claimed none of them: a live session kept re-running `git_status` and
    /// `git_diff` on its own gitignored notes folder, reading 36 bytes of branch
    /// header and 2 bytes of `ok` as if they said something about its files.
    #[test]
    fn an_empty_git_answer_says_why() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join(".gitignore"), "notes.txt\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "hidden from git\n").unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "kept\n").unwrap();
        git_out(dir.path(), &["add", "-A"]);
        git_out(dir.path(), &["commit", "-m", "init"]);
        let ctx = ToolCtx::new(dir.path());

        let quiet = status(&ctx, &json!({"porcelain": true}));
        assert!(
            quiet.output.contains("gitignored paths never appear"),
            "a quiet tree must say which of the two it is: {}",
            quiet.output
        );

        let ignored = diff(&ctx, &json!({"target": "notes.txt"}));
        assert!(
            ignored.ok && ignored.output.contains("is gitignored"),
            "{}",
            ignored.output
        );
        let clean = diff(&ctx, &json!({"target": "tracked.txt"}));
        assert!(
            clean.output.contains("identical to its committed version"),
            "{}",
            clean.output
        );
        let whole = diff(&ctx, &json!({}));
        assert!(
            whole.output.contains("no unstaged changes"),
            "{}",
            whole.output
        );

        std::fs::write(dir.path().join("loose.txt"), "untracked\n").unwrap();
        let untracked = diff(&ctx, &json!({"target": "loose.txt"}));
        assert!(
            untracked.output.contains("is untracked"),
            "{}",
            untracked.output
        );
        let dirty = status(&ctx, &json!({"porcelain": true}));
        assert!(
            dirty.output.contains("loose.txt")
                && !dirty.output.contains("gitignored paths never appear"),
            "a tree with something to report keeps its report: {}",
            dirty.output
        );
    }

}
