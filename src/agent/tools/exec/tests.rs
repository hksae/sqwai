use super::*;

/// Background-job tests share one process-global registry, and
/// `kill_remaining_jobs` reaps every live job in it: tests that hold a
/// live job (or kill them all) serialize here, or parallel threads
/// interleave kills with reads and all five assert global state.
static BG_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn ctx() -> ToolCtx {
    ToolCtx::new(std::env::temp_dir())
}

/// The exit code travels with the outcome instead of living only in
/// the human-readable status line: receipts record the sourced code.
/// A command that writes a *finished* hundred-kilobyte stream and then hangs.
/// The producer is a separate process that exits (and therefore flushes) before
/// the hang starts, so what the reader holds at the deadline is deterministic —
/// unlike `echo MARKER; sleep`, whose bytes may still sit in the shell's buffer
/// when we kill it.
#[cfg(unix)]
fn bulk_output_then_hang_command() -> String {
    "seq 1 20000; sleep 30".to_string()
}

#[cfg(windows)]
fn bulk_output_then_hang_command() -> String {
    if ShellKind::detect() == ShellKind::Cmd {
        // `for /L` echoes through cmd itself, line by line, and the ping that
        // follows is a separate process that runs for ~30s.
        "for /L %i in (1,1,20000) do @echo %i& ping -n 31 127.0.0.1 >nul".to_string()
    } else {
        // Git Bash: same spelling as unix, with the path-free form above.
        "seq 1 20000; sleep 30".to_string()
    }
}

/// Issue #178: the deadline branch killed the child and answered
/// "output discarded" while every byte the command had already printed was
/// still held by the reader threads. A hung `cargo test` is exactly the case
/// where the last captured line is worth more than nothing.
#[test]
fn a_timed_out_command_returns_what_it_managed_to_print() {
    let mut c = ctx();
    let started = std::time::Instant::now();
    let outcome = bash(&mut c, &bulk_output_then_hang_command(), Some(1), false);
    let elapsed = started.elapsed();

    assert!(!outcome.ok, "a killed command is not a success");
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "the timeout was not honoured: took {elapsed:?}"
    );
    assert!(
        outcome.output.contains("timed out"),
        "the status line must say what happened: {:?}",
        outcome.output
    );
    assert!(
        !outcome.output.contains("output discarded"),
        "the partial output must survive the kill: {:?}",
        outcome.output
    );
    // the last line of the bulk stream, written before the hang, is the proof
    assert!(
        outcome.output.contains("20000"),
        "captured output was thrown away: {:?}",
        &outcome.output[..outcome.output.len().min(200)]
    );
}

/// Issue #172: the reader capped each stream at the first 1 MB and dropped the
/// rest, so a long build lost precisely the failing line at its end. The budget
/// is now spent on head *and* tail.
#[test]
fn the_read_buffer_keeps_the_head_and_the_tail_and_drops_the_middle() {
    let mut b = HeadTail::new();
    let chunk: Vec<u8> = (0..8192u16).map(|i| (i % 251) as u8).collect();
    // ~3 MB through a 1 MB budget
    for _ in 0..384 {
        b.push(&chunk);
    }
    let kept = b.bytes();
    assert!(
        b.dropped() > 0,
        "384 chunks of 8 KiB must overflow the 1 MB budget"
    );
    assert!(
        kept.len() <= 3 * HEAD_TAIL_KEEP,
        "the resident ceiling is the 1 MB kept budget plus one doubled tail window: {} bytes kept, {} dropped",
        kept.len(),
        b.dropped()
    );
    // head intact: the very first bytes of the very first chunk
    assert_eq!(&kept[..chunk.len()], chunk.as_slice(), "head preserved");
    // tail intact: the last chunk is still the last thing in the buffer
    assert_eq!(
        &kept[kept.len() - chunk.len()..],
        chunk.as_slice(),
        "the end of the stream survived — this is what the old head-only cap deleted"
    );
}

#[test]
fn a_small_stream_passes_through_the_read_buffer_untouched() {
    let mut b = HeadTail::new();
    b.push(b"first line\n");
    b.push(b"last error line\n");
    assert_eq!(b.dropped(), 0, "nothing was dropped");
    assert_eq!(b.bytes(), b"first line\nlast error line\n");
}

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
    // The runtime shell comes from SHELL/SQWAI_SHELL, not the OS: a Git
    // Bash environment makes spawn_command run bash here, where the cmd
    // `for /L` spelling is a syntax error. Mirror the same detection.
    if ShellKind::detect() == ShellKind::Cmd {
        format!(
            "for /L %i in (1,1,2400) do @echo %time%>>\"{}\" & @ping -n 1 -w 40 127.0.0.1",
            marker.display()
        )
    } else {
        let path = marker.display().to_string().replace('\\', "/");
        format!("for i in $(seq 1 2400); do date +%s%N >> \"{path}\"; sleep 0.05; done")
    }
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
fn oversized_output_keeps_head_and_tail_with_a_marker() {
    let text = "0123456789abcdefghij";
    let out = crate::agent::tools::trim_middle(text, 5);
    assert!(out.starts_with("0"), "head survives: {out}");
    assert!(out.ends_with("ghij"), "tail survives: {out}");
    assert!(out.contains("output truncated"), "{out}");
    assert!(!out.contains("bcdef"), "middle is cut: {out}");
    // fits: untouched, no marker
    assert_eq!(crate::agent::tools::trim_middle(text, 20), text);
}

/// App shutdown must not orphan live jobs: `kill_remaining_jobs`
/// ends everything still running and drops it from the registry,
/// while finished jobs are left for their normal reaping.
#[test]
fn shutdown_kills_live_jobs_but_keeps_finished_ones_listed() {
    let _bg = BG_SERIAL.lock().unwrap();
    let mut c = ctx();
    let started = bash(&mut c, &long_sleep_command(), None, true);
    assert!(started.ok, "{}", started.output);
    assert!(!running_commands().is_empty(), "job must be registered");
    let killed = kill_remaining_jobs();
    assert_eq!(killed, 1, "exactly the live job dies");
    assert!(running_commands().is_empty(), "registry must drain");
    assert_eq!(kill_remaining_jobs(), 0, "second call is a no-op");
}

/// Background jobs are registered, pollable and killable: the whole point
/// of `bash_output`/`bash_kill` is that a detached command is not a
/// fire-and-forget leak but something the model can observe and stop.
#[test]
fn background_jobs_can_be_polled_and_killed() {
    let _bg = BG_SERIAL.lock().unwrap();
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
    let _bg = BG_SERIAL.lock().unwrap();
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

fn spawn_bg_with_log(c: &mut ToolCtx, command: &str) -> (u64, std::path::PathBuf) {
    let started = bash(c, command, None, true);
    assert!(started.ok, "{}", started.output);
    let id: u64 = started
        .output
        .split("job ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|num| num.parse().ok())
        .expect("spawn result must carry a job id");
    let log = started
        .output
        .split("logs appended to ")
        .nth(1)
        .and_then(|rest| rest.split(". Wait").next())
        .map(std::path::PathBuf::from)
        .expect("spawn result must carry the log path");
    (id, log)
}

/// Reaped and killed jobs leave no log files: the registry entry and
/// its `%TEMP%/sqwai-bg/bg-*.out` go away together.
#[test]
fn reaped_and_killed_jobs_leave_no_log_files() {
    let _bg = BG_SERIAL.lock().unwrap();
    let mut c = ctx();
    // kill path: running job removed by bash_kill
    let (id, log) = spawn_bg_with_log(&mut c, &long_sleep_command());
    assert!(log.exists(), "log must exist while the job runs");
    let killed = bash_kill(&c, &serde_json::json!({"id": id}));
    assert!(killed.ok, "{}", killed.output);
    assert!(!log.exists(), "killed job must not leave its log behind");

    // reap path: finished job removed by the final read
    let (id, log) = spawn_bg_with_log(&mut c, "echo hi");
    std::thread::sleep(std::time::Duration::from_millis(600));
    let polled = bash_output(&c, &serde_json::json!({"id": id}));
    assert!(polled.ok, "{}", polled.output);
    assert!(polled.output.contains("finished"), "{}", polled.output);
    assert!(!log.exists(), "reaped job must not leave its log behind");
}

/// Reads are incremental: the first returns the tail, the next only
/// what landed since — a chatty log crosses the context exactly once.
#[test]
fn bash_output_returns_deltas_after_the_first_read() {
    let _bg = BG_SERIAL.lock().unwrap();
    let mut c = ctx();
    let id = spawn_bg(&mut c, &long_sleep_command());
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let first = bash_output(&c, &serde_json::json!({"id": id}));
    assert!(first.ok, "{}", first.output);
    assert!(
        first.output.contains("--- output tail ---"),
        "{}",
        first.output
    );

    std::thread::sleep(std::time::Duration::from_millis(2000));
    let second = bash_output(&c, &serde_json::json!({"id": id}));
    assert!(second.ok, "{}", second.output);
    assert!(
        second
            .output
            .contains("--- new output since the last read ---"),
        "{}",
        second.output
    );
    assert!(
        !second
            .output
            .contains("<no new output since the last read>"),
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
    assert!(
        again.output.contains("--- output tail ---"),
        "{}",
        again.output
    );

    let killed = bash_kill(&c, &serde_json::json!({"id": id}));
    assert!(killed.ok, "{}", killed.output);
}

/// wait_secs wakes on fresh output: one call instead of a poll loop.
#[test]
fn bash_output_wait_returns_when_bytes_land() {
    let _bg = BG_SERIAL.lock().unwrap();
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
    let _bg = BG_SERIAL.lock().unwrap();
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

/// Audit H15: a forgotten background job grows its log without bound;
/// the tail read must be bounded — `fs::read` on a 5 GB log allocated
/// all of it and OOM'd the process.
#[test]
fn tail_of_large_log_is_bounded() {
    let dir = tempfile::Builder::new()
        .prefix("sqwai-tail")
        .tempdir()
        .unwrap();
    let log = dir.path().join("bg.out");
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::File::create(&log).unwrap();
        // 5GB of zeros via metadata only (sparse); write a marker at the end
        f.set_len(5_000_000_000).unwrap();
        f.seek(SeekFrom::End(-8)).unwrap();
        f.write_all(b"TAIL-END").unwrap();
    }
    let started = std::time::Instant::now();
    let tail = tail_of_file(&log, 4096).expect("tail must be readable");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "tail read the whole file: {:?}",
        started.elapsed()
    );
    assert!(tail.contains("TAIL-END"), "{tail}");
    assert!(tail.contains("only the last"), "{tail}");
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
    let _bg = BG_SERIAL.lock().unwrap();
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
    let _bg = BG_SERIAL.lock().unwrap();
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
