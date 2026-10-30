//! Where a project's host-owned state lives: beside the sessions, not in the
//! project.
//!
//! The worktree keeps only what belongs to it — `spill` (disposable tool
//! output the model may read), `skills` (project inputs), `exports`
//! (artifacts the user asked for) and the project config override. The event
//! log, the plan, the checkpoints and the lock have to outlive a
//! `git clean -fdx`, and a copied checkout must not carry a second history,
//! so they live under `<data>/projects/<hash>/` — the key shape the shadow
//! store already used for `[undo].shadow = "user"`.

use std::path::{Path, PathBuf};

/// The data-dir key for one project: hash of its canonical path. Truncated
/// because the directory name is for humans reading `ls`, not for uniqueness
/// beyond collision resistance.
pub fn project_key(root: &Path) -> String {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    blake3::hash(canonical.to_string_lossy().as_bytes()).to_hex()[..16].to_string()
}

/// `<data>/projects/<key>`, created on first use.
///
/// The legacy in-project `.sqwai` remains the fallback only when the platform
/// cannot name a data directory at all: a degraded install keeps working, and
/// the app never silently picks a second home.
pub fn project_dir(root: &Path) -> PathBuf {
    let Some(base) = data_base() else {
        return root.join(".sqwai");
    };
    let dir = base.join("projects").join(project_key(root));
    if !dir.exists() {
        migrate_legacy(root, &dir);
        let _ = std::fs::create_dir_all(&dir);
    }
    dir
}

/// The data dir. Tests get their own base so a suite of temporary projects
/// does not leave hundreds of hash directories in the real one.
pub(crate) fn data_base() -> Option<PathBuf> {
    #[cfg(test)]
    {
        Some(std::env::temp_dir().join("sqwai-test-data"))
    }
    #[cfg(not(test))]
    {
        crate::config::data_dir().ok()
    }
}

pub fn events_dir(root: &Path) -> PathBuf {
    project_dir(root).join("events")
}

pub fn plans_dir(root: &Path) -> PathBuf {
    project_dir(root).join("plans")
}

pub fn checkpoints_dir(root: &Path) -> PathBuf {
    project_dir(root).join("checkpoints")
}

pub fn lock_dir(root: &Path) -> PathBuf {
    project_dir(root).join("lock")
}

/// Move the pre-move `.sqwai` state into the data dir, once, when the new
/// home does not exist yet.
///
/// Only the durable pieces move: the event log (and its pre-rename `journal`
/// tail), plans and checkpoints. `spill`, `skills`, `exports` and the project
/// config override belong to the worktree and stay. The lock is not migrated:
/// its files describe live processes, and a stale one would mark this run
/// read-only.
fn migrate_legacy(root: &Path, dir: &Path) {
    let legacy = root.join(".sqwai");
    if !legacy.is_dir() {
        return;
    }
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    for name in ["events", "journal", "plans", "checkpoints"] {
        let from = legacy.join(name);
        if !from.exists() {
            continue;
        }
        let to = dir.join(name);
        if std::fs::rename(&from, &to).is_ok() {
            continue;
        }
        // A rename cannot cross a volume; copy, and drop the old tree only
        // once the copy is there.
        if copy_tree(&from, &to).is_ok() {
            let _ = std::fs::remove_dir_all(&from);
        }
    }
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
