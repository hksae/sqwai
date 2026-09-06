#![allow(dead_code)]
//! Shadow git checkpoints taken before every mutating action (design §2.5).
//!
//! Implemented on libgit2 (`git2` crate), no shell-outs. A checkpoint is a
//! dangling commit whose tree mirrors the whole worktree (tracked + untracked,
//! gitignore respected), built through an isolated in-memory index — the
//! user's staging area and branches are never touched.
//!
//! Restoring is path-scoped: the caller passes the paths the host recorded as
//! its own writes, each with the hash the agent left it at. A path whose
//! content no longer matches that hash was changed outside sqwai and is left
//! alone. Undo must not be able to discard work it did not do.
//!
//! Every function takes the repository root explicitly — the agent works on
//! the project directory, never on whatever cwd the process happens to have.

use anyhow::{Context as _, Result};
use git2::{IndexAddOption, Oid, Repository, Signature};
use sha2::{Digest, Sha256};
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

/// One path to put back, with the hash the agent left it at when the host
/// recorded one. `None` means "restore unconditionally" — the caller could not
/// narrow the scope and has said so to the user.
#[derive(Debug, Clone)]
pub struct Target {
    pub path: String,
    pub agent_hash: Option<String>,
}

/// What a restore actually did, for the journal and the status line.
#[derive(Debug, Default)]
pub struct RestoreReport {
    /// put back from the snapshot
    pub restored: Vec<String>,
    /// created after the snapshot, so removed
    pub deleted: Vec<String>,
    /// changed outside sqwai since the agent wrote them; left untouched
    pub skipped: Vec<String>,
}

impl RestoreReport {
    pub fn touched(&self) -> Vec<String> {
        let mut all = self.restored.clone();
        all.extend(self.deleted.iter().cloned());
        all
    }
}

/// Sha-256 of a file's bytes, matching what the file tools record in
/// `file_diff.hash_after`. `None` when the file is gone.
fn current_hash(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Some(format!("{:x}", hasher.finalize()))
}

/// Restore exactly `targets` from a snapshot, and nothing else.
///
/// A target present in the snapshot is written back byte for byte; one that is
/// absent from it appeared afterwards and is deleted. A target whose current
/// content does not match `agent_hash` was edited outside sqwai after the agent
/// wrote it, and is reported as skipped rather than overwritten (§2.5).
///
/// The index and HEAD are never touched, and no path outside `targets` is read
/// or written.
pub fn restore_paths(root: &Path, sha: &str, targets: &[Target]) -> Result<RestoreReport> {
    let repo = Repository::open(root).context("not a git repository")?;
    let oid: Oid = sha
        .parse()
        .with_context(|| format!("invalid snapshot sha: {sha}"))?;
    let commit = repo
        .find_commit(oid)
        .with_context(|| format!("snapshot commit not found: {sha}"))?;
    let snap_tree = commit.tree().context("snapshot tree")?;

    let mut report = RestoreReport::default();
    for target in targets {
        let relative = Path::new(&target.path);
        let absolute = root.join(relative);
        let live = current_hash(&absolute);

        // Only skip when the host knows what it left behind AND the file is
        // still there: a file the agent deleted has no hash to compare.
        if let (Some(expected), Some(live)) = (&target.agent_hash, &live)
            && expected != live
        {
            report.skipped.push(target.path.clone());
            continue;
        }

        match snap_tree.get_path(relative) {
            Ok(entry) => {
                let blob = repo
                    .find_blob(entry.id())
                    .with_context(|| format!("snapshot blob for {}", target.path))?;
                if let Some(parent) = absolute.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                // bytes as recorded, no git filters (§2.5: core.autocrlf = false)
                std::fs::write(&absolute, blob.content())
                    .with_context(|| format!("restoring {}", target.path))?;
                report.restored.push(target.path.clone());
            }
            Err(_) => {
                // not in the snapshot: it appeared after, so undo removes it
                if absolute.exists() {
                    std::fs::remove_file(&absolute)
                        .with_context(|| format!("removing {}", target.path))?;
                }
                report.deleted.push(target.path.clone());
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Run git against the fixture repository in an isolated environment.
    ///
    /// The fixture must not depend on the machine's git configuration: a
    /// global or system config can carry `init.templateDir`, `core.hooksPath`
    /// or a signing setup that makes a bare `git init` fail, which is how this
    /// test died on the macOS runner while passing everywhere else. `HOME`
    /// points at the fixture, system config is off, and the identity and
    /// initial branch are passed per invocation instead of being written into
    /// the repository by follow-up `git config` calls.
    fn git_in(dir: &Path, args: &[&str]) -> String {
        let o = std::process::Command::new("git")
            .current_dir(dir)
            .env("HOME", dir)
            .env("XDG_CONFIG_HOME", dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", dir.join("absent.gitconfig"))
            .env("GIT_TERMINAL_PROMPT", "0")
            .args([
                "-c",
                "init.defaultBranch=main",
                "-c",
                "user.name=sqwai-test",
                "-c",
                "user.email=test@sqwai.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("could not spawn git {args:?} in {}: {e}", dir.display()));
        // Report why, not just that: a bare "git failed" in CI is undebuggable.
        assert!(
            o.status.success(),
            "git {args:?} in {} exited with {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            dir.display(),
            o.status,
            String::from_utf8_lossy(&o.stdout).trim(),
            String::from_utf8_lossy(&o.stderr).trim(),
        );
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    /// A fresh fixture repository that cleans itself up when dropped.
    ///
    /// The name used to be built from `SystemTime::now().as_nanos()`. Cargo
    /// runs the tests in this module on parallel threads, and clock
    /// granularity is not uniform across platforms: where two calls land in
    /// the same tick, both tests get the *same* directory and then race on
    /// `.git`. `tempfile` allocates the directory atomically instead, so the
    /// fixtures cannot collide, and a panicking test no longer leaves it
    /// behind.
    fn tmp_repo() -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix("sqwai-ckpt-test-")
            .tempdir()
            .expect("temp dir for the fixture repository");
        let path = dir.path();
        git_in(path, &["init", "-q"]);
        fs::write(path.join("a.txt"), "hello\n").unwrap();
        git_in(path, &["add", "."]);
        git_in(path, &["commit", "-qm", "init"]);
        dir
    }

    /// Sha-256 as the file tools record it in `file_diff.hash_after`.
    fn hash_of(path: &Path) -> String {
        let mut hasher = Sha256::new();
        hasher.update(std::fs::read(path).unwrap());
        format!("{:x}", hasher.finalize())
    }

    /// Undo must not be able to discard work it did not do.
    ///
    /// Before this was scoped, `restore` force-checked-out the whole snapshot
    /// tree: a file the user was editing in their own editor while the agent
    /// worked was reverted along with everything else, and the edit was gone
    /// with no way back — it never was in any checkpoint.
    #[test]
    fn restore_leaves_a_file_the_agent_never_wrote_alone() {
        let repo = tmp_repo();
        let dir = repo.path();
        std::fs::write(dir.join("human.txt"), "human original\n").unwrap();
        git_in(dir, &["add", "."]);
        git_in(dir, &["commit", "-qm", "add human.txt"]);

        let sha = snapshot(dir, "pre_mutation a.txt").expect("snapshot");
        // the agent rewrites a.txt ...
        std::fs::write(dir.join("a.txt"), "agent edit\n").unwrap();
        let agent_hash = hash_of(&dir.join("a.txt"));
        // ... while the user works on a file the agent never touched
        std::fs::write(dir.join("human.txt"), "HUMAN WORK IN PROGRESS\n").unwrap();

        let report = restore_paths(
            dir,
            &sha,
            &[Target {
                path: "a.txt".into(),
                agent_hash: Some(agent_hash),
            }],
        )
        .expect("restore");

        assert_eq!(report.restored, vec!["a.txt".to_string()]);
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "hello\n",
            "the agent's own edit must be reverted"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("human.txt")).unwrap(),
            "HUMAN WORK IN PROGRESS\n",
            "the user's concurrent edit must survive undo"
        );
    }

    /// A file the agent created after the snapshot is not in the snapshot tree,
    /// so undo removes it. Force checkout used to leave it behind: the edits
    /// rolled back and the new files stayed in the worktree.
    #[test]
    fn restore_removes_files_created_after_the_snapshot() {
        let repo = tmp_repo();
        let dir = repo.path();
        let sha = snapshot(dir, "pre_mutation").expect("snapshot");
        std::fs::write(dir.join("generated.rs"), "// agent\n").unwrap();
        let agent_hash = hash_of(&dir.join("generated.rs"));

        let report = restore_paths(
            dir,
            &sha,
            &[Target {
                path: "generated.rs".into(),
                agent_hash: Some(agent_hash),
            }],
        )
        .expect("restore");

        assert_eq!(report.deleted, vec!["generated.rs".to_string()]);
        assert!(!dir.join("generated.rs").exists());
    }

    /// When the user edited the agent's own file afterwards, undo reports it
    /// and keeps its hands off. Silently overwriting would be the same data
    /// loss in a narrower scope.
    #[test]
    fn restore_skips_a_file_edited_after_the_agent_wrote_it() {
        let repo = tmp_repo();
        let dir = repo.path();
        let sha = snapshot(dir, "pre_mutation a.txt").expect("snapshot");
        std::fs::write(dir.join("a.txt"), "agent edit\n").unwrap();
        let agent_hash = hash_of(&dir.join("a.txt"));
        std::fs::write(dir.join("a.txt"), "agent edit, then mine\n").unwrap();

        let report = restore_paths(
            dir,
            &sha,
            &[Target {
                path: "a.txt".into(),
                agent_hash: Some(agent_hash),
            }],
        )
        .expect("restore");

        assert_eq!(report.skipped, vec!["a.txt".to_string()]);
        assert!(report.restored.is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "agent edit, then mine\n"
        );
    }

    /// The unscoped fallback, used when the host has no file records for a
    /// checkpoint (a `bash` mutation): restore what it is told to, no hash
    /// check. The caller warns the user that the scope is not narrowed.
    #[test]
    fn restore_without_a_recorded_hash_puts_the_file_back() {
        let repo = tmp_repo();
        let dir = repo.path();
        let sha = snapshot(dir, "pre_bash").expect("snapshot");
        std::fs::write(dir.join("a.txt"), "touched by a shell command\n").unwrap();

        let report = restore_paths(
            dir,
            &sha,
            &[Target {
                path: "a.txt".into(),
                agent_hash: None,
            }],
        )
        .expect("restore");

        assert_eq!(report.restored, vec!["a.txt".to_string()]);
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "hello\n"
        );
    }

    #[test]
    fn snapshot_and_restore_revert_edits_and_creations() {
        let repo = tmp_repo();
        let dir = repo.path();
        // state the agent will come back to
        fs::write(dir.join("a.txt"), "changed\n").unwrap();
        fs::write(dir.join("new.txt"), "created\n").unwrap();

        let sha = snapshot(dir, "test").expect("snapshot");

        // what the agent then did: rewrote two files and created a third
        fs::write(dir.join("a.txt"), "worse\n").unwrap();
        let a_hash = hash_of(&dir.join("a.txt"));
        fs::remove_file(dir.join("new.txt")).unwrap();
        fs::write(dir.join("extra.txt"), "extra\n").unwrap();
        let extra_hash = hash_of(&dir.join("extra.txt"));

        let report = restore_paths(
            dir,
            &sha,
            &[
                Target {
                    path: "a.txt".into(),
                    agent_hash: Some(a_hash),
                },
                Target {
                    path: "new.txt".into(),
                    // deleted by the agent: nothing on disk to compare against
                    agent_hash: None,
                },
                Target {
                    path: "extra.txt".into(),
                    agent_hash: Some(extra_hash),
                },
            ],
        )
        .expect("restore");

        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "changed\n");
        assert_eq!(
            fs::read_to_string(dir.join("new.txt")).unwrap(),
            "created\n",
            "a file the agent deleted comes back"
        );
        assert!(
            !dir.join("extra.txt").exists(),
            "a file the agent created is removed, not left behind"
        );
        assert_eq!(report.deleted, vec!["extra.txt".to_string()]);
        assert!(report.skipped.is_empty());

        // the git index must still equal HEAD (user staging untouched)
        let staged = git_in(dir, &["diff", "--cached", "--name-only"]);
        assert!(staged.trim().is_empty(), "staging area was modified");

        // branch must still be HEAD (no commit attached)
        let log = git_in(dir, &["log", "--oneline"]);
        assert_eq!(log.lines().count(), 1, "extra commit leaked onto branch");
    }

    #[test]
    fn available_detects_repo() {
        let repo = tmp_repo();
        let dir = repo.path();
        assert!(available(dir));
        let plain = dir.join("nested");
        fs::create_dir_all(&plain).unwrap();
        assert!(!available(&plain));
    }
}
