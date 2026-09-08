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
                match (my_created, other_created) {
                    (Some(my_t), Some(other_t)) if other_t < my_t => {
                        read_only = true;
                        break;
                    }
                    (Some(my_t), Some(other_t)) if other_t == my_t => {
                        if other_path.file_name() < path.file_name() {
                            read_only = true;
                            break;
                        }
                    }
                    _ => {
                        read_only = true;
                        break;
                    }
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
}
