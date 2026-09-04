#![allow(dead_code)]
//! Shadow git checkpoints taken before every mutating action (design §6).
//!
//! Implemented on libgit2 (`git2` crate), no shell-outs. A checkpoint is a
//! dangling commit whose tree mirrors the whole worktree (tracked + untracked,
//! gitignore respected), built through an isolated in-memory index — the
//! user's staging area and branches are never touched. Undo reverse-applies
//! the snapshot-vs-workdir diff to the working directory only; untracked
//! files created after the snapshot survive by design (matching `git diff
//! <sha>` semantics).
//!
//! Every function takes the repository root explicitly — the agent works on
//! the project directory, never on whatever cwd the process happens to have.

use anyhow::{Context as _, Result};
use git2::{IndexAddOption, Oid, Repository, Signature};
use std::path::Path;

/// true when checkpoints are possible in this directory
pub fn available(root: &Path) -> bool {
    Repository::open(root).is_ok()
}

/// create a shadow commit of the whole worktree (tracked + untracked),
/// returning its sha; HEAD, branches and the index stay untouched
pub fn snapshot(root: &Path, label: &str) -> Result<String> {
    let repo = Repository::open(root).context("not a git repository")?;

    // build the worktree tree using the repo's index in memory only: add_all
    // gathers tracked+untracked (gitignore-respecting), write_tree_to emits a
    // tree object; the on-disk index is never written, so the user's staging
    // area is untouched.
    let mut idx = repo.index().context("open repo index")?;
    idx.add_all(["."], IndexAddOption::DEFAULT, None)
        .with_context(|| format!("index-add worktree in {}", root.display()))?;
    let tree_oid = idx.write_tree_to(&repo).context("write tree")?;
    let tree = repo.find_tree(tree_oid).context("find tree")?;

    let sig = Signature::now("sqwai", "sqwai@local").context("signature")?;
    let parents: Vec<git2::Commit> = repo
        .head()
        .ok()
        .and_then(|h| h.peel_to_commit().ok())
        .into_iter()
        .collect();
    let parents_refs: Vec<&git2::Commit> = parents.iter().collect();

    // `None` ref => dangling commit, no branch/HEAD move
    let oid = repo
        .commit(
            None,
            &sig,
            &sig,
            &format!("sqwai checkpoint: {label}"),
            &tree,
            &parents_refs,
        )
        .context("create checkpoint commit")?;

    Ok(oid.to_string())
}

/// Return paths changed when restoring to a snapshot.
pub fn changed_files(root: &Path, sha: &str) -> Result<Vec<String>> {
    let repo = Repository::open(root).context("not a git repository")?;
    let oid: Oid = sha.parse().context("invalid snapshot sha")?;
    let commit = repo.find_commit(oid).context("snapshot commit not found")?;
    let tree = commit.tree().context("snapshot tree")?;
    let diff = repo
        .diff_tree_to_workdir(Some(&tree), None)
        .context("diff snapshot vs workdir")?;
    Ok(diff
        .deltas()
        .filter_map(|delta| delta.new_file().path().or(delta.old_file().path()))
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .collect())
}

/// restore the worktree to a snapshot, leaving the index and HEAD untouched;
/// untracked files created after the snapshot are preserved
pub fn restore(root: &Path, sha: &str) -> Result<()> {
    let repo = Repository::open(root).context("not a git repository")?;
    let oid: Oid = sha
        .parse()
        .with_context(|| format!("invalid snapshot sha: {sha}"))?;
    let commit = repo
        .find_commit(oid)
        .with_context(|| format!("snapshot commit not found: {sha}"))?;
    let snap_tree = commit.tree().context("snapshot tree")?;

    // early-out when the worktree already matches the snapshot
    let diff = repo
        .diff_tree_to_workdir(Some(&snap_tree), None)
        .context("diff snapshot vs workdir")?;
    if diff.deltas().len() == 0 {
        return Ok(());
    }

    // overwrite the worktree from the snapshot tree. FORCE overwrites even
    // locally-modified files; update_index(false) keeps the user's staging
    // area intact; no remove_untracked, so files created after the snapshot
    // survive.
    let mut cb = git2::build::CheckoutBuilder::new();
    cb.force().update_index(false).disable_filters(true);
    repo.checkout_tree(&snap_tree.into_object(), Some(&mut cb))
        .context("checkout snapshot into worktree")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn git_in(dir: &PathBuf, args: &[&str]) -> String {
        let o = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(o.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn tmp_repo() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sqwai-ckpt-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        git_in(&dir, &["init", "-q"]);
        git_in(&dir, &["config", "user.email", "t@t"]);
        git_in(&dir, &["config", "user.name", "t"]);
        fs::write(dir.join("a.txt"), "hello\n").unwrap();
        git_in(&dir, &["add", "."]);
        git_in(&dir, &["commit", "-qm", "init"]);
        dir
    }

    #[test]
    fn snapshot_and_restore_revert_edits_and_creations() {
        let dir = tmp_repo();
        // mutate an existing file and create a new one
        fs::write(dir.join("a.txt"), "changed\n").unwrap();
        fs::write(dir.join("new.txt"), "created\n").unwrap();

        let sha = snapshot(&dir, "test").expect("snapshot");

        // make further damage after the snapshot
        fs::write(dir.join("a.txt"), "worse\n").unwrap();
        fs::write(dir.join("extra.txt"), "extra\n").unwrap();
        fs::remove_file(dir.join("new.txt")).unwrap();

        restore(&dir, &sha).expect("restore");

        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "changed\n");
        assert_eq!(
            fs::read_to_string(dir.join("new.txt")).unwrap(),
            "created\n"
        );
        // files created AFTER the snapshot are not part of the undo
        assert_eq!(
            fs::read_to_string(dir.join("extra.txt")).unwrap(),
            "extra\n"
        );

        // the git index must still equal HEAD (user staging untouched)
        let staged = git_in(&dir, &["diff", "--cached", "--name-only"]);
        assert!(staged.trim().is_empty(), "staging area was modified");

        // branch must still be HEAD (no commit attached)
        let log = git_in(&dir, &["log", "--oneline"]);
        assert_eq!(log.lines().count(), 1, "extra commit leaked onto branch");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn available_detects_repo() {
        let dir = tmp_repo();
        assert!(available(&dir));
        let plain = dir.join("nested");
        fs::create_dir_all(&plain).unwrap();
        assert!(!available(&plain));
        let _ = fs::remove_dir_all(dir);
    }
}
