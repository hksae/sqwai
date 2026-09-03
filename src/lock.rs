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

        let session = Uuid::new_v4().to_string();
        let path = lock_dir.join(format!("{session}.lock"));
        let existing = lock_files(&lock_dir)?;
        let read_only = !existing.is_empty() && !force;

        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("cannot create project lock {}", path.display()))?;
        writeln!(file, "pid={}", std::process::id())?;
        writeln!(file, "session={session}")?;
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
}
