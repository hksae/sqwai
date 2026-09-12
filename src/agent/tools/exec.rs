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
    pub child: std::process::Child,
    /// owning session (#197): one process can host several sessions
    /// (switches, subagents) — each sees and kills only its own jobs
    pub session: String,
    /// cached once the process exits: the child is reaped (no zombie on
    /// Unix) but the job stays listed until `bash_output` reports it once
    pub exit: Option<std::process::ExitStatus>,
    /// log bytes already delivered to the model: `bash_output` is
    /// incremental — first read returns the tail, later reads only the
    /// bytes appended since the previous read, so chatty logs cross the
    /// context exactly once
    pub read: u64,
    /// consecutive reads without `wait_secs` while still running: drives
    /// the forced-wait escalation (models poll out of habit otherwise)
    pub nowait_polls: u32,
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

/// BELOW_NORMAL_PRIORITY_CLASS (0x4000), named here because winapi is not
/// a dependency and std exposes only the raw flag setter.
#[cfg(windows)]
const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
/// DETACHED_PROCESS (0x08000000), same story.
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0800_0000;

/// build a Command runnable in `cwd`
fn spawn_command(ctx: &ToolCtx, command: &str, cwd: Option<&str>) -> Command {
    let (kind, program, flag) = shell();
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Agent-spawned compiles/tests must never starve the TUI: below
        // normal priority, set here so every spawn site inherits it.
        // (Combined with DETACHED_PROCESS for background jobs below —
        // one call per Command, since flags replace rather than OR.)
        c.creation_flags(BELOW_NORMAL_PRIORITY_CLASS);
        if kind == ShellKind::Cmd {
            // cmd.exe /C does not follow CommandLineToArgvW quoting rules:
            // passing the command through .arg() re-quotes it, so embedded
            // quotes (paths with spaces, quoted arguments) arrive mangled
            // and cmd reports "not recognized as an internal or external
            // command". raw_arg hands the string to cmd.exe verbatim.
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
        exit_code: code,
        diff: None,
        file_diff: None,
        file_diffs: Vec::new(),
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
        cmd.creation_flags(DETACHED_PROCESS | BELOW_NORMAL_PRIORITY_CLASS);
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
                session: ctx.session_id.clone(),
                exit: None,
                read: 0,
                nowait_polls: 0,
            });
            Outcome::ok(format!(
                "launched in background as job {id} (pid {pid}) — logs appended to {}. \
                 Wait with bash_output(id, wait_secs), stop with bash_kill(id).",
                log.display()
            ))
        }
        Err(e) => Outcome::err(format!("background spawn failed: {e}")),
    }
}

/// Resolve one of THIS session's jobs (#197). A foreign id is reported
/// as foreign rather than missing, so the model learns the boundary
/// instead of retrying a kill/read loop against someone else's job.
fn own_job<'a>(
    jobs: &'a mut Vec<BgJob>,
    session: &str,
    id: u64,
) -> Result<&'a mut BgJob, String> {
    if jobs.iter().any(|j| j.id == id && j.session != session) {
        return Err(format!("job {id} belongs to another session"));
    }
    jobs
        .iter_mut()
        .find(|j| j.id == id)
        .ok_or_else(|| {
            format!("no background job {id} — use bash_output without an id to list jobs")
        })
}

/// Escalation schedule for no-wait reads of a running job: two free
/// reads, then forced waits — 15s the first time, 30s capped after.
/// Pure so tests pin the schedule without sleeping.
fn forced_wait_secs(consecutive_nowait: u32) -> u64 {
    match consecutive_nowait {
        0..=2 => 0,
        3 => 15,
        _ => 30,
    }
}

/// `bash_output`: incremental output of a background job, or a status list
/// of all jobs when no id is given. The first read of a job returns the
/// tail of its log; every later read returns only the bytes appended since
/// the previous read, so a chatty log crosses the context exactly once.
/// `from_start=true` re-reads the tail from scratch. `wait_secs` (0-60,
/// clamped) parks until the job exits or the timeout lapses — intermediate
/// output never wakes it early (that degenerates into polling on chatty
/// jobs); the read after the wait returns everything accumulated. Reads
/// without it on a running job are free twice, then force-waited (15s,
/// then 30s capped). A finished job is reported once with its exit code,
/// then removed from the registry.
pub(super) fn bash_output(ctx: &ToolCtx, args: &serde_json::Value) -> Outcome {
    if ctx.cancel_requested() {
        return Outcome::cancelled();
    }
    poll_jobs();
    let id = args["id"].as_u64();
    let mut jobs = match bg_jobs().lock() {
        Ok(j) => j,
        Err(_) => return Outcome::err("background job registry is unavailable"),
    };
    let Some(id) = id else {
        // #197: only this session's jobs are listed — other sessions'
        // jobs keep running, but they are invisible (and untouchable) here
        let mut lines = Vec::new();
        for job in jobs.iter().filter(|j| j.session == ctx.session_id) {
            lines.push(format!(
                "job {} — {} — `{}` (log: {})",
                job.id,
                job.status_line(),
                job.command,
                job.log.display()
            ));
        }
        if lines.is_empty() {
            return Outcome::ok("no background jobs");
        }
        return Outcome::ok(format!(
            "{} background job(s) (no id given — pass one for the output):\n{}",
            lines.len(),
            lines.join("\n")
        ));
    };

    let wait_secs = args["wait_secs"].as_u64().unwrap_or(0).clamp(0, 60);
    let from_start = args["from_start"].as_bool().unwrap_or(false);
    let tail = args["tail"].as_u64().unwrap_or(BG_TAIL_DEFAULT as u64) as usize;
    let tail = tail.clamp(200, BG_TAIL_MAX);

    // no-wait escalation: two free reads, then the host waits on the
    // model's behalf (15s, then 30s capped) — polling out of habit still
    // returns, just after a wait. An explicit wait_secs resets the count.
    let mut forced = 0u64;
    {
        let job = match own_job(&mut jobs, &ctx.session_id, id) {
            Ok(job) => job,
            Err(e) => return Outcome::err(e),
        };
        job.poll();
        if job.running() {
            if wait_secs > 0 {
                job.nowait_polls = 0;
            } else {
                job.nowait_polls += 1;
                forced = forced_wait_secs(job.nowait_polls);
            }
        }
    }
    let effective_wait = wait_secs.max(forced);

    // blocking wait BEFORE the read: until the job exits, the timeout
    // lapses, or Esc. Intermediate output NEVER wakes a wait: on a chatty
    // job "wake on output" degenerates into polling every couple of
    // seconds, which is exactly what wait exists to prevent. The read
    // after the wait returns everything accumulated. The registry lock
    // is never held across a sleep — the job is re-resolved after.
    if effective_wait > 0 {
        // the outer guard must go BEFORE the loop: it re-locks below, and
        // std Mutex is not reentrant — holding both would self-deadlock
        drop(jobs);
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(effective_wait);
        loop {
            if ctx.cancel_requested() {
                return Outcome::cancelled();
            }
            let done = match bg_jobs().lock() {
                Err(_) => return Outcome::err("background job registry is unavailable"),
                Ok(mut guard) => {
                    let job = match own_job(&mut guard, &ctx.session_id, id) {
                        Ok(job) => job,
                        Err(e) => return Outcome::err(format!("{e} — it was killed or reaped while waiting")),
                    };
                    job.poll();
                    !job.running()
                }
            };
            if done || std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        // re-lock for the read below
        jobs = match bg_jobs().lock() {
            Ok(j) => j,
            Err(_) => return Outcome::err("background job registry is unavailable"),
        };
    }

    let job = match own_job(&mut jobs, &ctx.session_id, id) {
        Ok(job) => job,
        Err(e) => return Outcome::err(e),
    };
    let status = job.status_line();
    let len = std::fs::metadata(&job.log).map(|m| m.len()).unwrap_or(0);
    let (body, first_read) = if from_start || job.read == 0 {
        // tail of the whole log, as before; the cursor parks at the end so
        // the next read is a delta from here
        let body = tail_of_file(&job.log, tail).unwrap_or_else(|| "<no output yet>".to_string());
        job.read = len;
        (body, true)
    } else if len > job.read {
        // only what landed since the previous read, bounded by `tail`
        let mut f = match std::fs::File::open(&job.log) {
            Ok(f) => f,
            Err(_) => return Outcome::err(format!("cannot read job {id} log")),
        };
        use std::io::{Read, Seek, SeekFrom};
        let start = job.read.max(len.saturating_sub(tail as u64));
        let mut buf = Vec::new();
        let body = match f
            .seek(SeekFrom::Start(start))
            .and_then(|_| f.read_to_end(&mut buf))
        {
            Ok(_) => {
                let skipped = start.saturating_sub(job.read);
                let mut text = String::from_utf8_lossy(&buf).into_owned();
                if skipped > 0 {
                    text = format!("…(showing the last {tail} of {skipped} new bytes)\n{text}");
                }
                text
            }
            Err(_) => return Outcome::err(format!("cannot read job {id} log")),
        };
        job.read = len;
        (body, false)
    } else {
        ("<no new output since the last read>".to_string(), false)
    };
    let log = job.log.display().to_string();
    let still_running = job.running();
    let polls = job.nowait_polls;
    if !still_running {
        jobs.retain(|j| j.id != id);
    }
    let section = if first_read {
        "--- output tail ---"
    } else {
        "--- new output since the last read ---"
    };
    // The hint rides right under the status line, never at the end: long
    // outputs get truncated from the tail, which is exactly where an
    // end-positioned hint dies unseen. Models poll out of habit even
    // though the schema documents wait_secs — a running job polled
    // without it says so itself, every time, until the habit breaks.
    // After two free reads the wait is forced (15s, then 30s capped).
    // A forced wait that ends at the exit still says so: otherwise the
    // model never learns it was parked.
    let notice = if wait_secs == 0 && (still_running || forced > 0) {
        if forced > 0 {
            format!("(no-wait poll #{polls} in a row: waited {forced}s for exit on your behalf — pass wait_secs yourself next time)\n")
        } else {
            "(do not poll in a loop: pass wait_secs (up to 60) to block until fresh output or exit)\n".to_string()
        }
    } else {
        String::new()
    };
    Outcome::ok(format!(
        "job {id}: {status} (log: {log})\n{notice}{section}\n{body}",
    ))
}

/// `bash_kill`: end a background job's whole process tree and report the
/// outcome. Only the owning session's jobs (#197). The job is removed from
/// the registry either way.
pub(super) fn bash_kill(ctx: &ToolCtx, args: &serde_json::Value) -> Outcome {
    poll_jobs();
    let Some(id) = args["id"].as_u64() else {
        return Outcome::err("bash_kill requires the job id (from bash background=true)");
    };
    let mut jobs = match bg_jobs().lock() {
        Ok(j) => j,
        Err(_) => return Outcome::err("background job registry is unavailable"),
    };
    let pos = match own_job(&mut jobs, &ctx.session_id, id) {
        Ok(_) => jobs.iter().position(|j| j.id == id).expect("resolved above"),
        Err(e) => return Outcome::err(e),
    };
    let mut job = jobs.remove(pos);
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

/// `sleep`: block up to 60s so the agent can wait for something outside
/// its control (a file, a server, a human) without burning turns on empty
/// polls. For background jobs prefer `bash_output(id, wait_secs)`: it wakes
/// on fresh output instead of sleeping blind. Sleeps in slices so Esc
/// cancels the wait like any other tool call.
pub(super) fn sleep(ctx: &ToolCtx, args: &serde_json::Value) -> Outcome {
    let secs = args["seconds"].as_u64().unwrap_or(0).clamp(0, 60);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        if ctx.cancel_requested() {
            return Outcome::cancelled();
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Outcome::ok(format!("slept {secs}s"))
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

    /// The exit code travels with the outcome instead of living only in
    /// the human-readable status line: receipts record the sourced code.
    #[test]
    fn exit_codes_travel_with_the_outcome() {
        let mut c = ctx();
        let ok_run = bash(&mut c, "echo hi", Some(30), false);
        assert!(ok_run.ok, "{}", ok_run.output);
        assert_eq!(ok_run.exit_code, Some(0));
        let bad_run = bash(&mut c, "exit 3", Some(30), false);
        assert!(!bad_run.ok);
        assert_eq!(bad_run.exit_code, Some(3));
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
        let list = bash_output(&c, &serde_json::json!({}));
        assert!(list.ok, "{}", list.output);
        assert!(
            list.output.contains(&format!("job {id}")),
            "{}",
            list.output
        );

        // give the pings a moment, then read the tail: output grows
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let polled = bash_output(&c, &serde_json::json!({"id": id}));
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
        let killed = bash_kill(&c, &serde_json::json!({"id": id}));
        assert!(killed.ok, "{}", killed.output);
        let gone = bash_output(&c, &serde_json::json!({"id": id}));
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

        let polled = bash_output(&c, &serde_json::json!({"id": id}));
        assert!(polled.ok, "{}", polled.output);
        assert!(polled.output.contains("finished"), "{}", polled.output);
        assert!(polled.output.contains("hi"), "{}", polled.output);

        let kill_late = bash_kill(&c, &serde_json::json!({"id": id}));
        assert!(!kill_late.ok, "{}", kill_late.output);
        assert!(
            kill_late.output.contains("no background job"),
            "{}",
            kill_late.output
        );
    }

    fn spawn_bg(c: &mut ToolCtx, command: &str) -> u64 {
        let started = bash(c, command, None, true);
        assert!(started.ok, "{}", started.output);
        started
            .output
            .split("job ")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|num| num.parse().ok())
            .expect("spawn result must carry a job id")
    }

    /// Reads are incremental: the first returns the tail, the next only
    /// what landed since — a chatty log crosses the context exactly once.
    #[test]
    fn bash_output_returns_deltas_after_the_first_read() {
        let mut c = ctx();
        let id = spawn_bg(&mut c, &long_sleep_command());
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let first = bash_output(&c, &serde_json::json!({"id": id}));
        assert!(first.ok, "{}", first.output);
        assert!(first.output.contains("--- output tail ---"), "{}", first.output);

        std::thread::sleep(std::time::Duration::from_millis(2000));
        let second = bash_output(&c, &serde_json::json!({"id": id}));
        assert!(second.ok, "{}", second.output);
        assert!(
            second.output.contains("--- new output since the last read ---"),
            "{}",
            second.output
        );
        assert!(
            !second.output.contains("<no new output since the last read>"),
            "ping must have written in 2s: {}",
            second.output
        );
        assert!(
            second.output.contains("do not poll in a loop"),
            "a running job polled without wait_secs must nudge: {}",
            second.output
        );

        // from_start re-reads the tail instead of the delta
        let again = bash_output(&c, &serde_json::json!({"id": id, "from_start": true}));
        assert!(again.ok, "{}", again.output);
        assert!(again.output.contains("--- output tail ---"), "{}", again.output);

        let killed = bash_kill(&c, &serde_json::json!({"id": id}));
        assert!(killed.ok, "{}", killed.output);
    }

    /// wait_secs wakes on fresh output: one call instead of a poll loop.
    #[test]
    fn bash_output_wait_returns_when_bytes_land() {
        let mut c = ctx();
        let id = spawn_bg(&mut c, "echo hello-wait");
        let t0 = std::time::Instant::now();
        let out = bash_output(&c, &serde_json::json!({"id": id, "wait_secs": 10}));
        assert!(out.ok, "{}", out.output);
        assert!(out.output.contains("hello-wait"), "{}", out.output);
        assert!(
            !out.output.contains("do not poll in a loop"),
            "no nudge when wait_secs was used: {}",
            out.output
        );
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(8),
            "wait must wake early, not sleep the full timeout"
        );
    }

    /// wait_secs gives up at the timeout while the job keeps running.
    #[test]
    fn bash_output_wait_times_out_on_a_quiet_job() {
        let mut c = ctx();
        let id = spawn_bg(&mut c, &long_sleep_command());
        // drain the startup burst so the wait has nothing fresh to wake on
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let _ = bash_output(&c, &serde_json::json!({"id": id}));
        let t0 = std::time::Instant::now();
        let out = bash_output(&c, &serde_json::json!({"id": id, "wait_secs": 1}));
        assert!(out.ok, "{}", out.output);
        assert!(out.output.contains("still running"), "{}", out.output);
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(5),
            "wait must respect its timeout"
        );
        let killed = bash_kill(&c, &serde_json::json!({"id": id}));
        assert!(killed.ok, "{}", killed.output);
    }

    #[test]
    fn forced_wait_schedule_is_two_free_then_15_then_30() {
        assert_eq!(forced_wait_secs(0), 0);
        assert_eq!(forced_wait_secs(1), 0);
        assert_eq!(forced_wait_secs(2), 0);
        assert_eq!(forced_wait_secs(3), 15);
        assert_eq!(forced_wait_secs(4), 30);
        assert_eq!(forced_wait_secs(99), 30);
    }

    /// Reads without wait_secs on a running job escalate: the 3rd forces
    /// a wait that sits until exit or cap (intermediate output does not
    /// wake it), the 4th would force 30s (pinned by the schedule unit
    /// test — not slept out here).
    #[test]
    fn repeated_nowait_reads_force_a_wait() {
        let mut c = ctx();
        // ~5s of output, then exit: the forced wait wakes on the exit,
        // not on the full 15s cap
        #[cfg(unix)]
        let cmd = "ping -c 6 127.0.0.1";
        #[cfg(windows)]
        let cmd = "ping -n 6 127.0.0.1";
        let id = spawn_bg(&mut c, cmd);
        // three back-to-back reads: the job cannot finish that fast, so
        // all three observe it running and the 3rd forces a wait
        let j = |v: serde_json::Value| bash_output(&c, &v);
        let first = j(serde_json::json!({"id": id}));
        assert!(first.ok, "{}", first.output);
        assert!(!first.output.contains("waited "), "{}", first.output);
        let second = j(serde_json::json!({"id": id}));
        assert!(second.ok, "{}", second.output);
        assert!(!second.output.contains("waited "), "{}", second.output);
        // third: forced 15s, waking on the ~5s exit instead
        let t0 = std::time::Instant::now();
        let third = j(serde_json::json!({"id": id}));
        assert!(third.ok, "{}", third.output);
        assert!(third.output.contains("waited 15s"), "{}", third.output);
        assert!(
            t0.elapsed() > std::time::Duration::from_secs(3),
            "a forced wait must actually park, not return instantly"
        );
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(14),
            "forced wait must wake on exit, not sleep the cap"
        );
    }

    #[test]
    fn sleep_waits_and_reports() {
        let c = ctx();
        let t0 = std::time::Instant::now();
        let out = sleep(&c, &serde_json::json!({"seconds": 1}));
        assert!(out.ok, "{}", out.output);
        assert_eq!(out.output, "slept 1s");
        assert!(
            t0.elapsed() >= std::time::Duration::from_millis(900),
            "must actually wait"
        );
        let zero = sleep(&c, &serde_json::json!({"seconds": 0}));
        assert!(zero.ok, "{}", zero.output);
    }

    /// #197: one process can host several sessions — each sees, reads
    /// and kills only its own background jobs.
    #[test]
    fn background_jobs_are_isolated_by_session() {
        let mut a = ctx();
        a.session_id = "session-a".into();
        let mut b = ctx();
        b.session_id = "session-b".into();
        let started = bash(&mut a, "echo hello-a", None, true);
        assert!(started.ok, "{}", started.output);
        let id: u64 = started
            .output
            .split("job ")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|num| num.parse().ok())
            .expect("spawn result must carry a job id");

        // B sees nothing of A's job
        let list = bash_output(&b, &serde_json::json!({}));
        assert!(list.ok, "{}", list.output);
        assert!(
            list.output.contains("no background jobs"),
            "foreign jobs must stay invisible: {}",
            list.output
        );
        let read = bash_output(&b, &serde_json::json!({"id": id}));
        assert!(!read.ok, "{}", read.output);
        assert!(
            read.output.contains("another session"),
            "read must name the boundary: {}",
            read.output
        );
        let kill = bash_kill(&b, &serde_json::json!({"id": id}));
        assert!(!kill.ok, "{}", kill.output);
        assert!(
            kill.output.contains("another session"),
            "kill must name the boundary: {}",
            kill.output
        );

        // ...while A works with it normally
        std::thread::sleep(std::time::Duration::from_millis(600));
        let polled = bash_output(&a, &serde_json::json!({"id": id}));
        assert!(polled.ok, "{}", polled.output);
        assert!(polled.output.contains("hello-a"), "{}", polled.output);
    }

    #[test]
    fn sleep_is_cancelled_promptly() {
        let c = ctx();
        let cancel = c.cancel.clone();
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let t0 = std::time::Instant::now();
        let out = sleep(&c, &serde_json::json!({"seconds": 60}));
        assert!(out.cancelled, "{}", out.output);
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(2),
            "a cancelled sleep must not run the clock"
        );
    }
}
