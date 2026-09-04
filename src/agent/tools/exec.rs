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

const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// bytes beyond which output is spilled to a temp file and only its tail returned
const MAX_RETURNED: usize = 30_000;

fn shell() -> (ShellKind, &'static str, &'static str) {
    let kind = ShellKind::detect();
    let (program, flag) = kind.program_and_flag();
    (kind, program, flag)
}

/// build a Command runnable in `cwd`
fn spawn_command(ctx: &ToolCtx, command: &str, cwd: Option<&str>) -> Command {
    let (_kind, program, flag) = shell();
    let mut c = Command::new(program);
    c.arg(flag).arg(command);
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
    let mut readers = Vec::new();
    let read_loop = |mut handle: Box<dyn std::io::Read + Send>, buf: Arc<Mutex<Vec<u8>>>| {
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match handle.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut b = buf.lock().unwrap();
                        if b.len() < 1_000_000 {
                            b.extend_from_slice(&chunk[..n]);
                        }
                    }
                }
            }
        })
    };
    if let Some(so) = child.stdout.take() {
        readers.push(read_loop(Box::new(so), out_buf.clone()));
    }
    if let Some(se) = child.stderr.take() {
        readers.push(read_loop(Box::new(se), err_buf.clone()));
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut status: Option<std::process::ExitStatus>;
    loop {
        status = child.try_wait().ok().flatten();
        if status.is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Outcome::err(format!(
                "command timed out after {timeout_secs}s — output discarded"
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    for r in readers {
        let _ = r.join();
    }

    let stdout = String::from_utf8_lossy(&out_buf.lock().unwrap()).into_owned();
    let stderr = String::from_utf8_lossy(&err_buf.lock().unwrap()).into_owned();
    let code = status.and_then(|s| s.code());
    let code_str = code.map(|c| c.to_string()).unwrap_or_else(|| "?".into());

    let mut combined = format!("{stdout}{stderr}");
    if combined.trim().is_empty() {
        combined = String::from("no output");
    }
    let status_line = format!("(exit code {code_str})");
    let body = if combined.len() > MAX_RETURNED {
        format!(
            "full output written to {}\n{}",
            spill(&combined).display(),
            tail_of(&combined, MAX_RETURNED)
        )
    } else {
        combined
    };
    let ok = code.map(|c| c == 0).unwrap_or(false);
    Outcome {
        ok,
        output: format!("{status_line}\n{body}"),
        diff: None,
        file_diff: None,
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
    match cmd.spawn() {
        Ok(child) => {
            let _ = child.id();
            Outcome::ok(format!(
                "launched in background — logs appended to {}",
                log.display()
            ))
        }
        Err(e) => Outcome::err(format!("background spawn failed: {e}")),
    }
}

fn tail_of(text: &str, wanted: usize) -> String {
    if text.len() <= wanted {
        text.to_string()
    } else {
        let mut cut = wanted;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
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
