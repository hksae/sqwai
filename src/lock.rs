use anyhow::{Context, Result};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub struct ProjectLock {
    path: PathBuf,
    pub read_only: bool,
    /// `--force` was given while another live instance still held a lock.
    forced_over_live: bool,
}

impl ProjectLock {
    pub fn acquire(root: &Path, force: bool) -> Result<Self> {
        let lock_dir = crate::agent::state::lock_dir(root);
        fs::create_dir_all(&lock_dir)
            .with_context(|| format!("cannot create lock directory {}", lock_dir.display()))?;

        prune_stale_locks(&lock_dir)?;

        let session = Uuid::new_v4().to_string();
        let path = lock_dir.join(format!("{session}.lock"));

        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("cannot create project lock {}", path.display()))?;
        writeln!(file, "pid={}", std::process::id())?;
        writeln!(file, "session={session}")?;
        // The PID alone cannot outlive recycling; its start identity can.
        if let Some(started) = process_start(std::process::id()) {
            writeln!(file, "started={started}")?;
        }

        // Determine writability after creating our lock file to prevent TOCTOU race
        let my_meta = fs::metadata(&path).ok();
        let my_created = my_meta.and_then(|m| m.created().or_else(|_| m.modified()).ok());

        let mut read_only = false;
        let mut forced_over_live = false;
        if force {
            forced_over_live = lock_files(&lock_dir)?
                .iter()
                .any(|other| other != &path && !lock_is_stale(other));
        }
        if !force {
            for other_path in lock_files(&lock_dir)? {
                if other_path == path {
                    continue;
                }
                let other_meta = fs::metadata(&other_path).ok();
                let other_created =
                    other_meta.and_then(|m| m.created().or_else(|_| m.modified()).ok());
                if is_lock_superseded(
                    my_created,
                    other_created,
                    path.file_name().unwrap_or_default(),
                    other_path.file_name().unwrap_or_default(),
                ) {
                    read_only = true;
                    break;
                }
            }
        }

        writeln!(file, "read_only={read_only}")?;

        Ok(Self {
            path,
            read_only,
            forced_over_live,
        })
    }

    pub fn status_message(&self) -> Option<String> {
        if self.read_only {
            return Some(
                "another sqwai instance owns this project; plan and state writes are read-only (use --force to override)".into(),
            );
        }
        self.forced_over_live.then(|| {
            "another sqwai instance still holds this project; continuing because --force was given".into()
        })
    }
}

pub(crate) fn is_lock_superseded(
    my_t: Option<std::time::SystemTime>,
    other_t: Option<std::time::SystemTime>,
    my_name: &std::ffi::OsStr,
    other_name: &std::ffi::OsStr,
) -> bool {
    match (my_t, other_t) {
        (Some(my), Some(other)) if other < my => true,
        (Some(my), Some(other)) if other == my => other_name < my_name,
        (Some(my), Some(other)) if other > my => false,
        _ => true,
    }
}

impl Drop for ProjectLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(windows)]
pub(crate) fn is_pid_alive(pid: u32) -> bool {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output();
    match out {
        Ok(out) => {
            let s = String::from_utf8_lossy(&out.stdout);
            s.contains(&format!("\"{pid}\""))
        }
        Err(_) => true,
    }
}

#[cfg(unix)]
pub(crate) fn is_pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn read_pid(path: &Path) -> Option<u32> {
    let content = fs::read_to_string(path).ok()?;
    for line in content.lines() {
        if let Some(pid_str) = line.strip_prefix("pid=") {
            return pid_str.trim().parse().ok();
        }
    }
    None
}

fn read_started(path: &Path) -> Option<u64> {
    let content = fs::read_to_string(path).ok()?;
    for line in content.lines() {
        if let Some(started) = line.strip_prefix("started=") {
            return started.trim().parse().ok();
        }
    }
    None
}

/// A lock is stale when its writer is gone: no pid, a dead pid, or — when
/// both sides can report it — a different start identity for that pid, which
/// is a recycled PID wearing a dead instance's number (audit L9). A lock
/// written before start identities existed is trusted while its pid lives.
fn lock_is_stale(path: &Path) -> bool {
    let Some(pid) = read_pid(path) else {
        return true;
    };
    if !is_pid_alive(pid) {
        return true;
    }
    match (read_started(path), process_start(pid)) {
        (Some(recorded), Some(current)) => recorded != current,
        _ => false,
    }
}

/// Start identity of a process: a PID can be reused, its creation time
/// cannot be reused by a different process. `None` when the platform cannot
/// say — the caller then trusts a live pid.
#[cfg(windows)]
pub(crate) fn process_start(pid: u32) -> Option<u64> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        let ok = GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user);
        CloseHandle(handle);
        (ok != 0).then_some(((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64)
    }
}

/// `/proc/<pid>/stat` field 22 (starttime, in clock ticks since boot). Field
/// 2 may contain spaces and parentheses, so parsing starts after the last
/// `)` — the field this reads is index 19 in what follows.
#[cfg(target_os = "linux")]
pub(crate) fn process_start(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(19))
        .and_then(|ticks| ticks.parse().ok())
}

#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) fn process_start(_pid: u32) -> Option<u64> {
    None
}

#[cfg(not(any(windows, unix)))]
pub(crate) fn process_start(_pid: u32) -> Option<u64> {
    None
}

/// Prune lock files whose recorded PID is no longer alive.
fn prune_stale_locks(dir: &Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "lock") && lock_is_stale(&path) {
            let _ = fs::remove_file(&path);
        }
    }
    Ok(())
}

fn lock_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut locks = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "lock") {
            locks.push(path);
        }
    }
    Ok(locks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn first_instance_is_writable_and_second_is_read_only() {
        let root = tempdir().unwrap();
        let first = ProjectLock::acquire(root.path(), false).unwrap();
        assert!(!first.read_only);
        let second = ProjectLock::acquire(root.path(), false).unwrap();
        assert!(second.read_only);
        assert!(second.status_message().unwrap().contains("read-only"));
    }

    #[test]
    fn force_allows_second_writable_instance() {
        let root = tempdir().unwrap();
        let _first = ProjectLock::acquire(root.path(), false).unwrap();
        let forced = ProjectLock::acquire(root.path(), true).unwrap();
        assert!(!forced.read_only);
    }

    #[test]
    fn dropping_lock_removes_its_file() {
        let root = tempdir().unwrap();
        let lock_path;
        {
            let lock = ProjectLock::acquire(root.path(), false).unwrap();
            lock_path = lock.path.clone();
            assert!(lock_path.exists());
        }
        assert!(!lock_path.exists());
    }

    #[test]
    fn stale_locks_from_dead_pids_are_pruned() {
        let root = tempdir().unwrap();
        let lock_dir = crate::agent::state::lock_dir(root.path());
        fs::create_dir_all(&lock_dir).unwrap();
        // A lock file with an unreachable PID
        let stale_path = lock_dir.join("stale.lock");
        fs::write(&stale_path, "pid=9999999\nsession=stale\nread_only=false\n").unwrap();
        assert!(stale_path.exists());

        let lock = ProjectLock::acquire(root.path(), false).unwrap();
        assert!(!lock.read_only);
        assert!(!stale_path.exists());
    }

    /// A recycled PID once kept a dead instance's project locked forever.
    /// The start identity distinguishes the process that wrote the file from
    /// whatever now wears its number.
    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn a_reused_pid_with_a_different_start_is_pruned() {
        let started = process_start(std::process::id()).expect("this process has a start identity");
        let root = tempdir().unwrap();
        let lock_dir = crate::agent::state::lock_dir(root.path());
        fs::create_dir_all(&lock_dir).unwrap();
        let stale_path = lock_dir.join("reused.lock");
        fs::write(
            &stale_path,
            format!(
                "pid={}\nsession=reused\nstarted={}\nread_only=false\n",
                std::process::id(),
                started.wrapping_add(1)
            ),
        )
        .unwrap();

        let lock = ProjectLock::acquire(root.path(), false).unwrap();
        assert!(!lock.read_only, "a reused pid must not lock the project");
        assert!(!stale_path.exists());
    }

    /// Locks written before start identities existed cannot prove a reuse;
    /// a live pid is the only evidence there is, and it is trusted.
    #[test]
    fn a_legacy_lock_with_a_live_pid_survives() {
        let root = tempdir().unwrap();
        let lock_dir = crate::agent::state::lock_dir(root.path());
        fs::create_dir_all(&lock_dir).unwrap();
        let legacy = lock_dir.join("legacy.lock");
        fs::write(
            &legacy,
            format!("pid={}\nsession=legacy\n", std::process::id()),
        )
        .unwrap();

        let lock = ProjectLock::acquire(root.path(), false).unwrap();
        assert!(lock.read_only, "a live legacy lock still owns the project");
        assert!(legacy.exists());
    }

    #[test]
    fn newer_lock_does_not_supersede_older_lock() {
        use std::ffi::OsStr;
        use std::time::{Duration, SystemTime};

        let now = SystemTime::now();
        let earlier = now - Duration::from_secs(10);
        let later = now + Duration::from_secs(10);

        let name_a = OsStr::new("a.lock");
        let name_b = OsStr::new("b.lock");

        // When another lock is newer than ours (other_t > my_t), our lock is NOT superseded.
        assert!(!is_lock_superseded(Some(now), Some(later), name_b, name_a));

        // When another lock is older than ours (other_t < my_t), our lock IS superseded.
        assert!(is_lock_superseded(Some(now), Some(earlier), name_b, name_a));

        // When timestamps are equal, tiebreak by filename.
        assert!(is_lock_superseded(Some(now), Some(now), name_b, name_a)); // other "a" < my "b" -> true
        assert!(!is_lock_superseded(Some(now), Some(now), name_a, name_b)); // other "b" > my "a" -> false

        // Missing timestamps fallback to safe default (superseded)
        assert!(is_lock_superseded(None, Some(now), name_a, name_b));
        assert!(is_lock_superseded(Some(now), None, name_a, name_b));
    }
}
