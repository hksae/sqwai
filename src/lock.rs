use anyhow::{Context, Result};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub struct ProjectLock {
    path: PathBuf,
    pub read_only: bool,
}

impl ProjectLock {
    pub fn acquire(root: &Path, force: bool) -> Result<Self> {
        let lock_dir = root.join(".sqwai").join("lock");
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

        // Determine writability after creating our lock file to prevent TOCTOU race
        let my_meta = fs::metadata(&path).ok();
        let my_created = my_meta.and_then(|m| m.created().or_else(|_| m.modified()).ok());

        let mut read_only = false;
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

        Ok(Self { path, read_only })
    }

    pub fn status_message(&self) -> Option<String> {
        self.read_only.then(|| {
            "another sqwai instance owns this project; plan/journal/memory/graph writes are read-only (use --force to override)".into()
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
fn is_pid_alive(pid: u32) -> bool {
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
fn is_pid_alive(pid: u32) -> bool {
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

/// Prune lock files whose recorded PID is no longer alive.
fn prune_stale_locks(dir: &Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "lock") {
            let is_stale = match read_pid(&path) {
                Some(pid) => pid != std::process::id() && !is_pid_alive(pid),
                None => true,
            };
            if is_stale {
                let _ = fs::remove_file(&path);
            }
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
        std::thread::sleep(std::time::Duration::from_millis(50));
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
        let lock_dir = root.path().join(".sqwai").join("lock");
        fs::create_dir_all(&lock_dir).unwrap();
        // A lock file with an unreachable PID
        let stale_path = lock_dir.join("stale.lock");
        fs::write(&stale_path, "pid=9999999\nsession=stale\nread_only=false\n").unwrap();
        assert!(stale_path.exists());

        let lock = ProjectLock::acquire(root.path(), false).unwrap();
        assert!(!lock.read_only);
        assert!(!stale_path.exists());
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
