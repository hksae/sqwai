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
                buf.lock().map(|mut b| b.extend_from_slice(&chunk[..n])).ok();
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
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(2);
            while readers.iter().any(|h: &std::thread::JoinHandle<()>| !h.is_finished())
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
    let output = match wait_git(ctx, &mut child, args.first().unwrap_or(&"?"), GIT_TIMEOUT_SECS) {
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
        ctx.checkpoint_chain(),
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
                    ctx.checkpoint_chain(),
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
                    ctx.checkpoint_chain(),
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
    let touched_files = extract_patch_files(ctx, patch);
    let mut resolved_paths = Vec::new();
    for f in &touched_files {
        match ctx.resolve(f) {
            Ok(p) => resolved_paths.push(p),
            Err(e) => return Outcome::err(format!("patch touches forbidden path '{f}': {e}")),
        }
    }

    let mut pre_images: Vec<(std::path::PathBuf, Option<Vec<u8>>)> = Vec::new();
    for p in &resolved_paths {
        let prev = std::fs::read(p).ok();
        pre_images.push((p.clone(), prev));
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
    // stdin written: drop our handle so EOF reaches git, then bounded wait
    drop(check.stdin.take());
    let checked = match wait_git(ctx, &mut check, "apply --check", GIT_TIMEOUT_SECS) {
        Ok(output) => output,
        Err(message) => return Outcome::err(format!("patch check failed: {message}")),
    };
    if !checked.status.success() {
        let error = super::decode_child_output(&checked.stderr);
        return Outcome::err(format!("patch rejected: {}", truncate(error.trim())));
    }

    let checkpoint = if let Ok(Some(sha)) = crate::agent::checkpoints::snapshot_session(
        &ctx.root,
        ctx.shadow_store,
        ctx.checkpoint_chain(),
        "patch",
    ) {
        ctx.journal.push((sha.clone(), "patch".to_string()));
        Some(sha)
    } else {
        None
    };

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

pub(super) fn extract_patch_files(ctx: &ToolCtx, patch: &str) -> Vec<String> {
    use std::io::Write;
    let mut cmd = match Command::new("git")
        .current_dir(&ctx.root)
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
    drop(cmd.stdin.take());
    let output = match wait_git(ctx, &mut cmd, "apply --numstat", GIT_TIMEOUT_SECS) {
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

/// `step_diff`: show what changed in a specific plan step from the shadow checkpoints.
pub fn step_diff(ctx: &ToolCtx, args: &Value) -> Outcome {
    let step_id = if let Some(s) = args.get("step_id").and_then(Value::as_str) {
        s.trim()
    } else if let Some(s) = args.get("id").and_then(Value::as_str) {
        s.trim()
    } else {
        ""
    };
    if step_id.is_empty() {
        return Outcome::err("step_id is required");
    }
    let target_path = args
        .get("path")
        .or_else(|| args.get("target"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // Check plan if present
    if let Ok(Some(plan)) = crate::plan::open_active_for_session(&ctx.root, Some(&ctx.session_id))
        && let Some(step) = plan.step(step_id)
        && step.status == crate::plan::StepStatus::Pending
    {
        return Outcome::ok(format!(
            "step '{step_id}' is pending and has not been started yet"
        ));
    }

    let Some(shadow) = crate::agent::checkpoints::shadow_repo(&ctx.root, ctx.shadow_store) else {
        return Outcome::err("shadow checkpoint repository is not available");
    };

    // If step is currently in progress, ensure current worktree is snapshotted
    let _ = crate::agent::checkpoints::snapshot_boundary(
        &ctx.root,
        ctx.shadow_store,
        ctx.checkpoint_chain(),
        &format!("step_{step_id}_probe"),
    );

    // Find commits in the session chain
    let commits = match shadow.commit_log(ctx.checkpoint_chain()) {
        Ok(c) if !c.is_empty() => c,
        _ => match shadow.commit_log("shared") {
            Ok(c) => c,
            Err(e) => return Outcome::err(format!("reading shadow history failed: {e:#}")),
        },
    };

    let (mut start_sha, mut finish_sha) = resolve_boundary_commits(&commits, step_id);

    // Fallback to journal records if labels not in commit messages
    if (start_sha.is_none() || finish_sha.is_none())
        && let Ok(records) = crate::agent::journal::Journal::records_for(&ctx.root, &ctx.session_id)
    {
        for r in &records {
            let matches_step = r.step.as_deref() == Some(step_id)
                || r.fields.get("step").and_then(Value::as_str) == Some(step_id)
                || (r.kind == "plan"
                    && r.fields.get("id").and_then(Value::as_str) == Some(step_id));
            if !matches_step {
                continue;
            }
            if let Some(sha) = r.fields.get("id").and_then(Value::as_str) {
                let reason = r.fields.get("reason").and_then(Value::as_str);
                if reason == Some("step_start") {
                    start_sha = Some(sha.to_string());
                    finish_sha = None;
                }
                if reason == Some("step_finish") && start_sha.is_some() {
                    finish_sha = Some(sha.to_string());
                }
            }
            if let Some(sha) = r.fields.get("checkpoint").and_then(Value::as_str) {
                start_sha = Some(sha.to_string());
                finish_sha = None;
            }
        }
    }

    // If finish is still not found, use latest commit on the session chain
    if finish_sha.is_none() {
        finish_sha = shadow
            .head_of(ctx.checkpoint_chain())
            .or_else(|| shadow.head_of("shared"));
    }

    let (Some(start), Some(finish)) = (start_sha, finish_sha) else {
        return Outcome::err(format!(
            "no checkpoint boundaries found for step '{step_id}'"
        ));
    };

    match shadow.diff(&start, &finish, target_path) {
        Ok(diff) => {
            let trimmed = diff.trim();
            if trimmed.is_empty() {
                if let Some(path) = target_path {
                    Outcome::ok(format!("no changes to '{path}' in step {step_id}"))
                } else {
                    Outcome::ok(format!("no changes recorded for step {step_id}"))
                }
            } else {
                Outcome::ok(trimmed.to_string())
            }
        }
        Err(e) => Outcome::err(format!("git diff failed: {e:#}")),
    }
}

pub(crate) fn resolve_boundary_commits(
    commits: &[(String, String)],
    step_id: &str,
) -> (Option<String>, Option<String>) {
    let start_label = format!("step_{step_id}_start");
    let finish_label = format!("step_{step_id}_finish");

    let mut start_sha: Option<String> = None;
    let mut finish_sha: Option<String> = None;

    // commits are newest first (reverse chronological order)
    for (sha, label) in commits {
        if label.contains(&finish_label) && finish_sha.is_none() && start_sha.is_none() {
            finish_sha = Some(sha.clone());
        }
        if label.contains(&start_label) && start_sha.is_none() {
            start_sha = Some(sha.clone());
        }
    }
    (start_sha, finish_sha)
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
    fn resolve_boundary_commits_handles_reopened_steps() {
        // Step was finished (cycle 1), reopened, and started again (cycle 2 in progress)
        // Commits in reverse-chronological order (newest first):
        let commits = vec![
            ("sha_work".into(), "commit: work in progress".into()),
            ("sha_start_2".into(), "checkpoint: step_1_start".into()),
            ("sha_finish_1".into(), "checkpoint: step_1_finish".into()),
            ("sha_start_1".into(), "checkpoint: step_1_start".into()),
        ];
        let (start, finish) = resolve_boundary_commits(&commits, "1");
        assert_eq!(start.as_deref(), Some("sha_start_2"));
        // finish must NOT pair with stale sha_finish_1 from prior cycle
        assert_eq!(finish, None);

        // Once cycle 2 finishes:
        let commits_finished = vec![
            ("sha_finish_2".into(), "checkpoint: step_1_finish".into()),
            ("sha_start_2".into(), "checkpoint: step_1_start".into()),
            ("sha_finish_1".into(), "checkpoint: step_1_finish".into()),
            ("sha_start_1".into(), "checkpoint: step_1_start".into()),
        ];
        let (start, finish) = resolve_boundary_commits(&commits_finished, "1");
        assert_eq!(start.as_deref(), Some("sha_start_2"));
        assert_eq!(finish.as_deref(), Some("sha_finish_2"));
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
}
