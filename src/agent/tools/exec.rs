#![allow(dead_code)]
//! Shell execution tool (`bash`).
//!
//! Cross-platform: uses the platform default shell (cmd on Windows, sh
//! elsewhere). Supports an optional timeout (kills on expiry), a background
//! mode that detaches the process and writes all output to a temp file, and
//! truncates very long output to a tail plus the path of the full log file.

use super::{Outcome, ToolCtx};
use crate::agent::shell::ShellKind;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// bytes beyond which output is spilled to a temp file and only its tail returned
const MAX_RETURNED: usize = 30_000;
/// default tail `bash_output` returns for one job
const BG_TAIL_DEFAULT: usize = 10_000;
const BG_TAIL_MAX: usize = 50_000;

/// A detached process started with `bash background=true`, kept so its output
/// can be read later (`bash_output`) and it can be killed or reaped
/// (`bash_kill`) instead of leaking as a zombie until the app exits.
pub(super) struct BgJob {
    pub id: u64,
    pub command: String,
    pub log: PathBuf,
    pub started: std::time::Instant,
    child: std::process::Child,
    /// cached once the process exits: the child is reaped (no zombie on
    /// Unix) but the job stays listed until `bash_output` reports it once
    exit: Option<std::process::ExitStatus>,
}

impl BgJob {
    /// Poll without blocking; caches the exit status when done.
    fn poll(&mut self) {
        if self.exit.is_none() {
            self.exit = self.child.try_wait().ok().flatten();
        }
    }

    fn running(&self) -> bool {
        self.exit.is_none()
    }

    fn status_line(&self) -> String {
        match self.exit.as_ref().and_then(|s| s.code()) {
            Some(code) => format!("finished (exit code {code})"),
            None if self.exit.is_some() => "finished".to_string(),
            None => format!("still running ({})", elapsed_of(self)),
        }
    }
}

fn bg_jobs() -> &'static Mutex<Vec<BgJob>> {
    static JOBS: OnceLock<Mutex<Vec<BgJob>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(Vec::new()))
}

static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(1);

/// Poll every job. This reaps exited children (the per-spawn zombie leak on
/// Unix) without removing them: a finished job stays listed until
/// `bash_output` reports it once.
fn poll_jobs() {
    if let Ok(mut jobs) = bg_jobs().lock() {
        for job in jobs.iter_mut() {
            job.poll();
        }
    }
}

fn shell() -> (ShellKind, &'static str, &'static str) {
    let kind = ShellKind::detect();
    let (program, flag) = kind.program_and_flag();
    (kind, program, flag)
}

/// build a Command runnable in `cwd`
fn spawn_command(ctx: &ToolCtx, command: &str, cwd: Option<&str>) -> Command {
    let (kind, program, flag) = shell();
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        if kind == ShellKind::Cmd {
            // cmd.exe /C does not follow CommandLineToArgvW quoting rules:
            // passing the command through .arg() re-quotes it, so embedded
            // quotes (paths with spaces, quoted arguments) arrive mangled
            // and cmd reports "not recognized as an internal or external
            // command". raw_arg hands the string to cmd.exe verbatim.
            use std::os::windows::process::CommandExt;
            c.arg(flag).raw_arg(command);
        } else {
            c.arg(flag).arg(command);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = kind;
        c.arg(flag).arg(command);
    }
    match cwd {
        Some(d) => {
            if let Ok(p) = ctx.resolve(d) {
                c.current_dir(p);
            }
        }
        None => {
            c.current_dir(&ctx.root);
        }
    }
    c
}

/// run to completion under a timeout; kill on expiry
pub(super) fn bash(
    ctx: &mut ToolCtx,
    command: &str,
    timeout: Option<u64>,
    background: bool,
) -> Outcome {
    if command.trim().is_empty() {
        return Outcome::err("empty command");
    }
    if background {
        return run_background(ctx, command);
    }
    let timeout_secs = timeout.unwrap_or(DEFAULT_TIMEOUT_SECS).max(1);
    run_blocking(ctx, command, timeout_secs)
}

/// Kill a child and drain its wait, ignoring errors: by the time this runs the
/// process may already be gone (it finished a moment before the flag was
/// checked), and that race is not a failure worth reporting.
fn kill_and_reap(child: &mut std::process::Child) {
    kill_tree(child);
    let _ = child.wait();
}

/// Join the pipe-reader threads, but not forever. The pipes close when the
/// tree dies, which lets the readers return — but a grandchild that somehow
/// survives the kill must not wedge the tool call: after the grace period the
/// handles are dropped (detached) and whatever is still blocked exits on its
/// own when its pipes finally close.
fn join_readers(readers: Vec<std::thread::JoinHandle<()>>) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut pending = readers;
    while !pending.is_empty() && std::time::Instant::now() < deadline {
        pending.retain(|handle| !handle.is_finished());
        if !pending.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

/// End the child and everything it started. `Child::kill` takes down only the
/// direct child, but a `cmd /C` shell command usually means grandchildren —
/// `ping`, `cargo`, compilers — that inherit the pipes and keep mutating the
/// project after the shell is gone, and keep our own pipe-reader join blocked
/// waiting on them. `taskkill /T` ends the whole tree instead.
#[cfg(windows)]
fn kill_tree(child: &mut std::process::Child) {
    // Only when it is still running: the pid could otherwise be recycled for
    // an unrelated process between the check and the kill.
    let running = child.try_wait().map(|s| s.is_none()).unwrap_or(true);
    if running {
        let done = std::process::Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !done {
            let _ = child.kill();
        }
    }
}

/// Unix shells usually `exec` a lone command, so the direct child is the
/// whole tree. Pipelines and background jobs can still outlive it — that
/// needs a process group (`setpgid` + `kill(-pgid)`), which is a follow-up,
/// not this change.
#[cfg(not(windows))]
fn kill_tree(child: &mut std::process::Child) {
    let _ = child.kill();
}

fn run_blocking(ctx: &ToolCtx, command: &str, timeout_secs: u64) -> Outcome {
    use std::io::Read;
    use std::sync::{Arc, Mutex};

    let mut cmd = spawn_command(ctx, command, None);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Outcome::err(format!("spawn failed: {e}")),
    };

    // read stdout/stderr concurrently so a chatty child can't deadlock
    let out_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let err_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let out_capped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let err_capped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut readers = Vec::new();
    let read_loop = |mut handle: Box<dyn std::io::Read + Send>,
                     buf: Arc<Mutex<Vec<u8>>>,
                     capped: Arc<std::sync::atomic::AtomicBool>| {
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match handle.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut b = buf.lock().unwrap();
                        if b.len() < 1_000_000 {
                            let available = 1_000_000 - b.len();
                            let to_take = n.min(available);
                            b.extend_from_slice(&chunk[..to_take]);
                            if to_take < n {
                                capped.store(true, std::sync::atomic::Ordering::Relaxed);
                            }
                        } else {
                            capped.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }
            }
        })
    };
    if let Some(so) = child.stdout.take() {
        readers.push(read_loop(Box::new(so), out_buf.clone(), out_capped.clone()));
    }
    if let Some(se) = child.stderr.take() {
        readers.push(read_loop(Box::new(se), err_buf.clone(), err_capped.clone()));
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut status: Option<std::process::ExitStatus>;
    loop {
        status = child.try_wait().ok().flatten();
        if status.is_some() {
            break;
        }
        // §3.7 / §7 S: Esc sets this from the TUI. Checked at the same
        // cadence as the timeout, because this loop is the only place a
        // long-running `bash` call can be interrupted at all — the tokio task
        // running the agent loop can be aborted, but that does not reach a
        // child process spawned on this blocking thread; without this check,
        // pressing Esc during `bash` gave the *illusion* of cancelling while
        // the command kept mutating the project unseen until it finished on
        // its own.
        if ctx.cancel_requested() {
            kill_and_reap(&mut child);
            // the readers still hold the pipe ends; joining them is what
            // notices the pipes closed and lets them return
            join_readers(readers);
            return Outcome::cancelled();
        }
        if std::time::Instant::now() >= deadline {
            kill_and_reap(&mut child);
            return Outcome::err(format!(
                "command timed out after {timeout_secs}s — output discarded"
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    join_readers(readers);

    let stdout = String::from_utf8_lossy(&out_buf.lock().unwrap()).into_owned();
    let stderr = String::from_utf8_lossy(&err_buf.lock().unwrap()).into_owned();
    let code = status.and_then(|s| s.code());
    let code_str = code.map(|c| c.to_string()).unwrap_or_else(|| "?".into());

    let mut combined = format!("{stdout}{stderr}");
    if combined.trim().is_empty() {
        combined = String::from("no output");
    }
    let status_line = format!("(exit code {code_str})");
    let is_capped = out_capped.load(std::sync::atomic::Ordering::Relaxed)
        || err_capped.load(std::sync::atomic::Ordering::Relaxed);
    let body = if combined.len() > MAX_RETURNED || is_capped {
        let note = if is_capped {
            format!(
                "output (capped at 2 MB) written to {}",
                spill(&combined).display()
            )
        } else {
            format!("full output written to {}", spill(&combined).display())
        };
        format!("{note}\n{}", tail_of(&combined, MAX_RETURNED))
    } else {
        combined
    };
    let ok = code.map(|c| c == 0).unwrap_or(false);
    Outcome {
        ok,
        output: format!("{status_line}\n{body}"),
        diff: None,
        file_diff: None,
        cancelled: false,
    }
}

fn run_background(ctx: &ToolCtx, command: &str) -> Outcome {
    let dir = std::env::temp_dir().join("sqwai-bg");
    let _ = std::fs::create_dir_all(&dir);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let log = dir.join(format!("bg-{stamp}.out"));

    let mut cmd = spawn_command(ctx, command, None);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // DETACHED_PROCESS
    }
    cmd.stdin(Stdio::null());
    if let Ok(f) = std::fs::File::create(&log) {
        cmd.stdout(Stdio::from(f));
        if let Ok(af) = std::fs::OpenOptions::new().append(true).open(&log)
            && let Ok(copy) = af.try_clone()
        {
            cmd.stderr(Stdio::from(copy));
        } else {
            cmd.stderr(Stdio::null());
        }
    } else {
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());
    }
    let id = NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed);
    match cmd.spawn() {
        Ok(child) => {
            let pid = child.id();
            let command_head: String = command.chars().take(120).collect();
            bg_jobs().lock().unwrap().push(BgJob {
                id,
                command: command_head,
                log: log.clone(),
                started: std::time::Instant::now(),
                child,
                exit: None,
            });
            Outcome::ok(format!(
                "launched in background as job {id} (pid {pid}) — logs appended to {}. \
                 Poll with bash_output(id), stop with bash_kill(id).",
                log.display()
            ))
        }
        Err(e) => Outcome::err(format!("background spawn failed: {e}")),
    }
}

/// `bash_output`: the tail of a background job's log, or a status list of all
/// jobs when no id is given. A finished job is reported once with its exit
/// code, then removed from the registry.
pub(super) fn bash_output(args: &serde_json::Value) -> Outcome {
    poll_jobs();
    let id = args["id"].as_u64();
    let mut jobs = match bg_jobs().lock() {
        Ok(j) => j,
        Err(_) => return Outcome::err("background job registry is unavailable"),
    };
    let Some(id) = id else {
        if jobs.is_empty() {
            return Outcome::ok("no background jobs");
        }
        let mut lines = Vec::new();
        for job in jobs.iter() {
            lines.push(format!(
                "job {} — {} — `{}` (log: {})",
                job.id,
                job.status_line(),
                job.command,
                job.log.display()
            ));
        }
        return Outcome::ok(format!(
            "{} background job(s) (no id given — pass one for the output tail):\n{}",
            lines.len(),
            lines.join("\n")
        ));
    };

    let Some(job) = jobs.iter_mut().find(|j| j.id == id) else {
        return Outcome::err(format!(
            "no background job {id} — use bash_output without an id to list jobs"
        ));
    };
    let status = job.status_line();
    let tail = args["tail"].as_u64().unwrap_or(BG_TAIL_DEFAULT as u64) as usize;
    let tail = tail.clamp(200, BG_TAIL_MAX);
    let body = tail_of_file(&job.log, tail).unwrap_or_else(|| "<no output yet>".to_string());
    let log = job.log.display().to_string();
    if !job.running() {
        jobs.retain(|j| j.id != id);
    }
    Outcome::ok(format!(
        "job {id}: {status} (log: {})\n--- output tail ---\n{body}",
        log
    ))
}

/// `bash_kill`: end a background job's whole process tree and report the
/// outcome. The job is removed from the registry either way.
pub(super) fn bash_kill(args: &serde_json::Value) -> Outcome {
    poll_jobs();
    let Some(id) = args["id"].as_u64() else {
        return Outcome::err("bash_kill requires the job id (from bash background=true)");
    };
    let mut jobs = match bg_jobs().lock() {
        Ok(j) => j,
        Err(_) => return Outcome::err("background job registry is unavailable"),
    };
    let Some(mut job) = jobs.iter().position(|j| j.id == id).map(|i| jobs.remove(i)) else {
        return Outcome::err(format!("no background job {id} to kill"));
    };
    job.poll();
    match job.exit {
        Some(status) => Outcome::ok(format!(
            "job {id} had already finished: `{}` (exit code {})",
            job.command,
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".into())
        )),
        None => {
            kill_tree(&mut job.child);
            let _ = job.child.wait();
            Outcome::ok(format!("job {id} killed: `{}`", job.command))
        }
    }
}

fn elapsed_of(job: &BgJob) -> String {
    let secs = job.started.elapsed().as_secs();
    if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// Last `max` bytes of a file, cut on a char boundary.
fn tail_of_file(path: &PathBuf, max: usize) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    if data.is_empty() {
        return None;
    }
    let start = if data.len() <= max {
        0
    } else {
        let mut cut = data.len() - max;
        // walk forward to the next UTF-8 char start (skip continuation bytes)
        while cut < data.len() && (data[cut] & 0b1100_0000) == 0b1000_0000 {
            cut += 1;
        }
        cut
    };
    let truncated = start > 0;
    let text = String::from_utf8_lossy(&data[start..]).into_owned();
    Some(if truncated {
        format!("…(only the last {max} bytes shown)\n{text}")
    } else {
        text
    })
}

fn tail_of(text: &str, wanted: usize) -> String {
    if text.len() <= wanted {
        text.to_string()
    } else {
        let mut cut = text.len().saturating_sub(wanted);
        while cut < text.len() && !text.is_char_boundary(cut) {
            cut += 1;
        }
        format!("…(output truncated, showing tail)\n{}", &text[cut..])
    }
}

fn spill(contents: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("sqwai-cmd");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!(
        "cmd-{}.out",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::File::create(&path).and_then(|mut f| f.write_all(contents.as_bytes()));
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolCtx {
        ToolCtx::new(std::env::temp_dir())
    }

    /// A command that blocks well past any test deadline on this platform and
    /// outputs lines steadily so background polling tests see output in the log.
    /// Both Unix and Windows pace 30 seconds of `ping` to localhost; either
    /// way a single process the kill ends.
    #[cfg(unix)]
    fn long_sleep_command() -> String {
        "ping -c 31 127.0.0.1".to_string()
    }

    #[cfg(windows)]
    fn long_sleep_command() -> String {
        "ping -n 31 127.0.0.1".to_string()
    }

    /// A loop that appends a timestamp line to `marker` about twenty times a
    /// second. Same platform split: `cmd` has no `sleep`/`seq`/`date`, so it
    /// loops with `for /L` and paces with `ping`. Single `%i` (a command
    /// line, not a batch file); the path is quoted for spaces. The loop runs
    /// for ~120s: far longer than any scheduling delay a loaded test machine
    /// can produce, so the elapsed bound below can only fail if the child
    /// really was left running.
    #[cfg(unix)]
    fn marker_loop_command(marker: &std::path::Path) -> String {
        format!(
            "for i in $(seq 1 2400); do date +%s%N >> {}; sleep 0.05; done",
            marker.display()
        )
    }

    #[cfg(windows)]
    fn marker_loop_command(marker: &std::path::Path) -> String {
        format!(
            "for /L %i in (1,1,2400) do @echo %time%>>\"{}\" & @ping -n 1 -w 40 127.0.0.1",
            marker.display()
        )
    }

    /// The bug this exists to fix: before the poll loop checked the cancel
    /// flag, aborting the surrounding tokio task did not reach a child
    /// process spawned on a `spawn_blocking` thread at all — `bash` kept
    /// running, invisibly, until its own timeout. This drives a command whose
    /// timeout is far longer than the test, flips the flag from another
    /// thread partway through, and asserts the call returns promptly with a
    /// cancelled outcome rather than only after the long timeout.
    #[test]
    fn cancelling_a_running_command_returns_promptly_as_cancelled() {
        let mut c = ctx();
        let cancel = c.cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        });

        let started = std::time::Instant::now();
        // a command that would otherwise run far longer than this test
        let outcome = bash(&mut c, &long_sleep_command(), Some(60), false);
        let elapsed = started.elapsed();

        assert!(
            outcome.cancelled,
            "ok={} output={:?}",
            outcome.ok, outcome.output
        );
        assert!(!outcome.ok);
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "cancellation did not interrupt the command: took {elapsed:?}"
        );
    }

    /// The point of killing the child rather than only giving up on it: a
    /// process left running after "cancellation" is worse than no
    /// cancellation at all, because nothing in the UI shows it is still
    /// mutating the project. Proven here by having the child write to a file
    /// repeatedly and checking that it stops the moment it is cancelled.
    #[test]
    fn the_child_process_is_actually_killed_not_abandoned() {
        let dir = tempfile::Builder::new()
            .prefix("sqwai-cancel")
            .tempdir()
            .unwrap();
        let marker = dir.path().join("alive");
        let mut c = ToolCtx::new(dir.path());
        let cancel = c.cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        });

        let command = marker_loop_command(&marker);
        let started = std::time::Instant::now();
        let outcome = bash(&mut c, &command, Some(60), false);
        let elapsed = started.elapsed();
        assert!(
            outcome.cancelled,
            "ok={} output={:?}",
            outcome.ok, outcome.output
        );
        // The real proof: `bash` must not block until the child exits on its
        // own (the loop runs for ~120s). If the process were merely abandoned
        // rather than killed, the pipe readers this call joins on would keep
        // it waiting for that full span; a working kill returns promptly even
        // on a loaded machine.
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "bash() blocked until the child finished on its own: {elapsed:?}"
        );

        let count_after_cancel = std::fs::read_to_string(&marker)
            .map(|s| s.lines().count())
            .unwrap_or(0);
        std::thread::sleep(std::time::Duration::from_millis(400));
        let count_later = std::fs::read_to_string(&marker)
            .map(|s| s.lines().count())
            .unwrap_or(0);
        assert_eq!(
            count_after_cancel, count_later,
            "the marker kept growing after cancellation — the child is still running"
        );
    }

    /// Without a cancellation, ordinary completion is unaffected: the flag
    /// starting `false` must not itself do anything.
    #[test]
    fn an_uncancelled_command_completes_normally() {
        let mut c = ctx();
        let outcome = bash(&mut c, "echo hi", Some(10), false);
        assert!(outcome.ok);
        assert!(!outcome.cancelled);
        assert!(outcome.output.contains("hi"));
    }

    #[test]
    fn tail_of_returns_requested_number_of_trailing_bytes() {
        let text = "0123456789abcdefghij";
        let tail = tail_of(text, 5);
        assert!(tail.ends_with("fghij"));
        assert!(tail.contains("output truncated"));
    }

    /// Background jobs are registered, pollable and killable: the whole point
    /// of `bash_output`/`bash_kill` is that a detached command is not a
    /// fire-and-forget leak but something the model can observe and stop.
    #[test]
    fn background_jobs_can_be_polled_and_killed() {
        let mut c = ctx();
        // a long command whose output lands in the job log steadily; no inner
        // redirection: cmd under DETACHED_PROCESS dies silently the moment
        // the command line carries its own `>` redirect (observed on Win11)
        let started = bash(&mut c, &long_sleep_command(), None, true);
        assert!(started.ok, "{}", started.output);
        let id: u64 = started
            .output
            .split("job ")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|num| num.parse().ok())
            .expect("spawn result must carry a job id");

        // list without an id shows the job
        let list = bash_output(&serde_json::json!({}));
        assert!(list.ok, "{}", list.output);
        assert!(
            list.output.contains(&format!("job {id}")),
            "{}",
            list.output
        );

        // give the pings a moment, then read the tail: output grows
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let polled = bash_output(&serde_json::json!({"id": id}));
        assert!(polled.ok, "{}", polled.output);
        assert!(
            polled.output.contains(&format!("job {id}")),
            "{}",
            polled.output
        );
        assert!(
            !polled.output.contains("<no output yet>"),
            "expected ping output in the tail: {}",
            polled.output
        );

        // kill; killed jobs are gone from the registry
        let killed = bash_kill(&serde_json::json!({"id": id}));
        assert!(killed.ok, "{}", killed.output);
        let gone = bash_output(&serde_json::json!({"id": id}));
        assert!(!gone.ok, "{}", gone.output);
    }

    /// A finished job reports its exit code once, is reaped, and a later
    /// `bash_kill` on the same id says so instead of pretending to kill.
    #[test]
    fn a_finished_job_is_reported_then_reaped() {
        let mut c = ctx();
        let started = bash(&mut c, "echo hi", None, true);
        assert!(started.ok, "{}", started.output);
        let id: u64 = started
            .output
            .split("job ")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|num| num.parse().ok())
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(600));

        let polled = bash_output(&serde_json::json!({"id": id}));
        assert!(polled.ok, "{}", polled.output);
        assert!(polled.output.contains("finished"), "{}", polled.output);
        assert!(polled.output.contains("hi"), "{}", polled.output);

        let kill_late = bash_kill(&serde_json::json!({"id": id}));
        assert!(!kill_late.ok, "{}", kill_late.output);
        assert!(
            kill_late.output.contains("no background job"),
            "{}",
            kill_late.output
        );
    }
}
