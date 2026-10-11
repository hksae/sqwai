//! Shell execution tool (`bash`).
//!
//! Cross-platform: uses the platform default shell (cmd on Windows, sh
//! elsewhere). Supports an optional timeout (kills on expiry), a background
//! mode that detaches the process and writes all output to a temp file, and
//! truncates very long output to a tail plus the path of the full log file.

use super::{Outcome, ToolCtx};
use crate::agent::shell::ShellKind;
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

/// (id, owning session, command) of every background job whose process is
/// still alive. Undo restores consult this: the writer lock stops
/// in-process dispatch, but an already-running shell keeps writing while
/// the tree is being reverted — so undo refuses until these are gone.
pub(super) fn running_commands() -> Vec<(u64, String, String)> {
    poll_jobs();
    bg_jobs()
        .lock()
        .map(|jobs| {
            jobs.iter()
                .filter(|job| job.running())
                .map(|job| (job.id, job.session.clone(), job.command.clone()))
                .collect()
        })
        .unwrap_or_default()
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

fn shell() -> (ShellKind, std::borrow::Cow<'static, str>, &'static str) {
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
    let mut c = Command::new(program.as_ref());
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
        use std::os::unix::process::CommandExt;
        // Own process group per spawn: grandchildren (pipelines, `&`
        // backgrounds) inherit it, so kill_tree can signal the whole tree
        // with kill(-pgid) instead of only the direct child.
        c.process_group(0);
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
/// Shared with git.rs: hung hooks get the same tree-kill, not a bare kill.
#[cfg(windows)]
pub(crate) fn kill_tree(child: &mut std::process::Child) {
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

/// Unix shells usually `exec` a lone command, so the direct child is often
/// the whole tree — but pipelines and `&` backgrounds outlive it. Every
/// spawn above runs in its own process group, so signal the group leader's
/// whole group; fall back to the direct child when the group is already
/// gone (pid reuse makes a blind group-kill unsafe, hence the liveness
/// check first — same discipline as the Windows branch).
#[cfg(not(windows))]
pub(crate) fn kill_tree(child: &mut std::process::Child) {
    let running = child.try_wait().map(|s| s.is_none()).unwrap_or(true);
    if !running {
        return;
    }
    // SAFETY: killpg with a valid live pid and SIGKILL has no failure mode
    // beyond ESRCH (raced exit), which falls back to child.kill() below.
    // The negative pid addresses only the child's own group, created at
    // spawn — never pid 1's group, never ours.
    let done = unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) } == 0;
    if !done {
        let _ = child.kill();
    }
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

    // console codepage, not UTF-8: plain lossy decoding turned RU
    // system messages into ���� (baselines and reports carried them)
    let stdout = super::decode_child_output(&out_buf.lock().unwrap());
    let stderr = super::decode_child_output(&err_buf.lock().unwrap());
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
        // One home for oversized output, and it is a home the file tools can
        // open. This used to be `%TEMP%\sqwai-cmd\`: a path outside the
        // project jail, so the promise in the note was one only `bash` could
        // keep — `read` would have refused it.
        let note = match (super::fs::spill_output(ctx, "cmd", &combined), is_capped) {
            (Some(rel), true) => format!("output (capped at 2 MB) written to {rel}"),
            (Some(rel), false) => format!("full output written to {rel}"),
            (None, _) => "output too large to show".to_string(),
        };
        format!("{note}\n{}", super::trim_middle(&combined, MAX_RETURNED))
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
fn own_job<'a>(jobs: &'a mut [BgJob], session: &str, id: u64) -> Result<&'a mut BgJob, String> {
    if jobs.iter().any(|j| j.id == id && j.session != session) {
        return Err(format!("job {id} belongs to another session"));
    }
    jobs.iter_mut().find(|j| j.id == id).ok_or_else(|| {
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
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(effective_wait);
        loop {
            if ctx.cancel_requested() {
                return Outcome::cancelled();
            }
            let done = match bg_jobs().lock() {
                Err(_) => return Outcome::err("background job registry is unavailable"),
                Ok(mut guard) => {
                    let job = match own_job(&mut guard, &ctx.session_id, id) {
                        Ok(job) => job,
                        Err(e) => {
                            return Outcome::err(format!(
                                "{e} — it was killed or reaped while waiting"
                            ));
                        }
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
        // reaped read: the registry entry goes away, and so does the log
        // file — finished logs are never listed, so keeping them only
        // grows %TEMP%/sqwai-bg/ without bound.
        let log_path = job.log.clone();
        jobs.retain(|j| j.id != id);
        let _ = std::fs::remove_file(&log_path);
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
            format!(
                "(no-wait poll #{polls} in a row: waited {forced}s for exit on your behalf — pass wait_secs yourself next time)\n"
            )
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
        Ok(_) => jobs
            .iter()
            .position(|j| j.id == id)
            .expect("resolved above"),
        Err(e) => return Outcome::err(e),
    };
    let mut job = jobs.remove(pos);
    job.poll();
    // the log goes with the registry entry, finished or killed
    let _ = std::fs::remove_file(&job.log);
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

/// End every still-running background job and drop it from the registry.
/// Called once on app shutdown: detached jobs would otherwise keep running
/// (and mutating the project) after the UI is gone. Finished jobs are left
/// for their normal `bash_output` reaping — exit only hurries the living.
/// Returns how many were killed.
pub(crate) fn kill_remaining_jobs() -> usize {
    poll_jobs();
    let mut jobs = match bg_jobs().lock() {
        Ok(jobs) => jobs,
        Err(_) => return 0,
    };
    let mut killed = 0;
    jobs.retain_mut(|job| {
        job.poll();
        if job.running() {
            kill_tree(&mut job.child);
            let _ = job.child.wait();
            killed += 1;
            // the process is exiting: an unread log nobody will ever open
            // must not stay behind in %TEMP% (audit H16)
            std::fs::remove_file(&job.log).ok();
            return false;
        }
        std::fs::remove_file(&job.log).ok();
        true
    });
    killed
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
    // Bounded read: seek to the tail instead of loading the whole file. A
    // forgotten `while true; do date; done` job grows its log for hours,
    // and `fs::read` on a multi-GB log allocated all of it (audit H15).
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len == 0 {
        return None;
    }
    let start = len.saturating_sub(max as u64);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut data: Vec<u8> = Vec::new();
    // take(): the job may still be writing — never read past the tail
    // window plus a small growth allowance
    (&mut file)
        .take(max as u64 + 4096)
        .read_to_end(&mut data)
        .ok()?;
    if data.is_empty() {
        return None;
    }
    let mut cut = 0usize;
    if start > 0 {
        // walk forward to the next UTF-8 char start (skip continuation bytes)
        while cut < data.len() && (data[cut] & 0b1100_0000) == 0b1000_0000 {
            cut += 1;
        }
    }
    let truncated = start > 0;
    let text = String::from_utf8_lossy(&data[cut..]).into_owned();
    Some(if truncated {
        format!("…(only the last {max} bytes shown)\n{text}")
    } else {
        text
    })
}

#[cfg(test)]
mod tests;
