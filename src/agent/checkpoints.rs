#![allow(dead_code)]
//! Checkpoints taken before mutating actions (§2.5), across two layers.
//!
//! Layer 1 ([`crate::agent::blobs`]) keeps the pre-image of every file the
//! host is about to write, needs no git, and powers `/undo step N`.
//!
//! Layer 2 ([`crate::agent::shadow`]) is the tree snapshot `bash` needs,
//! taken in a shadow repository of our own. It used to be taken as a dangling
//! commit in the *user's* repository, which §2.5 forbids and which a routine
//! `git gc` in their project silently invalidated (#17). Nothing here touches
//! their `.git` any more, and `git2` is gone from the dependency list per
//! §5.10 — git is a binary invoked with an explicit `--git-dir`.
//!
//! Restoring is path-scoped: the caller passes the paths the host recorded as
//! its own writes, each with the hash the agent left it at. A path whose
//! content no longer matches that hash was changed outside sqwai and is left
//! alone. Undo must not be able to discard work it did not do.
//!
//! Every function takes the project root explicitly — the agent works on the
//! project directory, never on whatever cwd the process happens to have.

use anyhow::{Context as _, Result};
use sha2::{Digest, Sha256};
use std::path::Path;

use crate::agent::shadow::Shadow;
use crate::config::ShadowStore;

/// Open the shadow repository, or `None` when layer 2 is unavailable (no git
/// binary, or `[undo].shadow = "off"`). Layer 1 does not go through here.
pub(crate) fn shadow(root: &Path, store: ShadowStore) -> Option<Shadow> {
    Shadow::open(root, store).ok().flatten()
}

pub fn shadow_repo(root: &Path, store: ShadowStore) -> Option<Shadow> {
    shadow(root, store)
}

/// True when a *tree* snapshot is possible — that is, when layer 2 could
/// operate here. It no longer means "this is a git repository": the shadow
/// repository is ours, so a snapshot needs the `git` binary and nothing else.
/// File reverts do not need even that, so callers must not read this as
/// "undo is available".
///
/// Deliberately does not create anything: an availability check that
/// initialised a repository as a side effect would litter every directory the
/// TUI asks about.
pub fn available(root: &Path) -> bool {
    crate::agent::shadow::git_available()
        && crate::agent::shadow::dir_for(root, ShadowStore::Local).is_some()
}

/// Snapshot the worktree onto this session's chain in the shadow repository
/// (§2.5), returning the commit.
///
/// The session id keys the chain (`refs/sessions/<id>`), so two sessions in
/// the same project do not interleave their history. An unchanged tree
/// produces no commit and returns `Ok(None)`.
pub fn snapshot_session(
    root: &Path,
    store: ShadowStore,
    session_id: &str,
    label: &str,
) -> Result<Option<String>> {
    let Some(shadow) = shadow(root, store) else {
        anyhow::bail!("no shadow repository: git is unavailable or [undo].shadow is off");
    };
    shadow.snapshot(session_id, label)
}

/// Snapshot the worktree for this session, returning the commit hash representing
/// this boundary (creating a boundary commit even if tree is unchanged).
pub fn snapshot_boundary(
    root: &Path,
    store: ShadowStore,
    session_id: &str,
    label: &str,
) -> Result<Option<String>> {
    let Some(shadow) = shadow(root, store) else {
        return Ok(None);
    };
    shadow.snapshot_forced(session_id, label).map(Some)
}

pub fn changed_files(root: &Path, sha: &str) -> Result<Vec<String>> {
    let Some(shadow) = shadow(root, ShadowStore::Local) else {
        anyhow::bail!("no shadow repository");
    };
    shadow.changed_files(sha)
}

/// What one maintenance pass did, for the status line and the journal.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Maintenance {
    /// session chains dropped because their journal is gone
    pub chains_dropped: Vec<String>,
    /// Length of the surviving session's chain, against
    /// `[undo].keep_per_session`. Reported rather than enforced — see the
    /// note on [`maintain`].
    pub kept_chain_len: usize,
    pub over_keep_limit: bool,
    /// layer-1 blobs removed and bytes freed
    pub blobs_removed: usize,
    pub blobs_freed_bytes: u64,
    /// the shadow repository was collected
    pub collected: bool,
}

impl Maintenance {
    pub fn did_anything(&self) -> bool {
        !self.chains_dropped.is_empty() || self.blobs_removed > 0 || self.collected
    }
}

/// Retention for both layers (§2.5), run when a session ends.
///
/// Layer 1: a blob is kept while any journal still names it, or while it is
/// younger than `[undo].blob_grace_secs`.
///
/// Layer 2: a session chain whose journal is gone has nothing left that could
/// reference its checkpoints, so its ref is dropped; `git gc --prune=now`
/// then collects what became unreachable. Collection also runs when the
/// shadow repository is over `[undo].shadow_max_bytes`, because that is the
/// only bound the user was given.
///
/// Never touches the chain of `keep_session`: that is the session asking for
/// the maintenance, and its own undo history has to survive it.
///
/// `[undo].keep_per_session` is **reported, not enforced**, and that is a
/// deliberate gap rather than an oversight. §2.5 stores checkpoints as a
/// commit chain, and dropping the oldest commits of a chain requires
/// rewriting every descendant — which changes the shas the session already
/// recorded in its journal and in plan evidence, turning working undo
/// references into dangling ones. Reporting the overrun keeps the promise
/// honest until the chain shape is revisited (see the PR that added this).
pub fn maintain(
    root: &Path,
    cfg: &crate::config::UndoConfig,
    keep_session: &str,
) -> Result<Maintenance> {
    let mut report = Maintenance::default();

    let referenced = crate::agent::journal::Journal::referenced_blobs(root)?;
    let purged = crate::agent::blobs::purge(
        root,
        &referenced,
        std::time::Duration::from_secs(cfg.blob_grace_secs),
    );
    report.blobs_removed = purged.removed;
    report.blobs_freed_bytes = purged.freed_bytes;

    let Some(shadow) = shadow(root, cfg.shadow) else {
        return Ok(report);
    };
    let alive = crate::agent::journal::Journal::sessions_on_disk(root);
    for session in shadow.sessions()? {
        if session == keep_session || alive.contains(&session) {
            continue;
        }
        if shadow.drop_session(&session).is_ok() {
            report.chains_dropped.push(session);
        }
    }
    report.kept_chain_len = shadow.chain_len(keep_session).unwrap_or(0);
    report.over_keep_limit = report.kept_chain_len > cfg.keep_per_session as usize;
    let oversized = shadow.size_bytes() > cfg.shadow_max_bytes;
    if !report.chains_dropped.is_empty() || oversized {
        shadow.gc()?;
        report.collected = true;
    }
    Ok(report)
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
    /// the host wrote them, but no pre-image is available to put back — a
    /// record from before the blob store, or a store that could not be
    /// written. Reported rather than guessed at.
    pub no_pre_image: Vec<String>,
}

impl RestoreReport {
    pub fn touched(&self) -> Vec<String> {
        let mut all = self.restored.clone();
        all.extend(self.deleted.iter().cloned());
        all
    }
}

/// Sha-256 of a file's bytes, in the same `sha256:`-prefixed format the file
/// tools record in `file_diff.hash_after`. `None` when the file is gone.
///
/// The prefix matters: comparing a raw hex digest against a journal record
/// never matches, which silently turned every scoped restore into a row of
/// skips while hand-rolled test hashes stayed green.
fn current_hash(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Some(format!("sha256:{:x}", hasher.finalize()))
}

/// Restore files from the layer-1 blob store, with no git involved at all.
///
/// This is what §2.5 means by "the constraint 'undo unavailable outside git
/// repositories' is lifted": every file the host wrote through
/// `write | edit | multi_edit | patch` has its pre-image in the store, keyed
/// from the journal, so putting it back is a copy.
///
/// The same outside-edit rule as the git path applies: a file whose current
/// content is not what the host last left is reported as skipped, never
/// overwritten. A pre-image of `None` means the file did not exist before the
/// undone window, so reverting it means removing it.
pub fn restore_from_blobs(
    root: &Path,
    pre_images: &[crate::agent::journal::PreImage],
) -> Result<RestoreReport> {
    let mut report = RestoreReport::default();
    for item in pre_images {
        let path = root.join(&item.path);
        let current = current_hash(&path);
        // Only skip when we know both what it should be and what it is, and
        // they disagree. A file that is already gone cannot be clobbered.
        if let (Some(expected), Some(actual)) = (item.agent_hash.as_deref(), current.as_deref())
            && expected != actual
        {
            report.skipped.push(item.path.clone());
            continue;
        }
        match &item.blob_before {
            _ if item.blob_before.is_none() && item.existed_before => {
                // The file was edited, not created, and its pre-image is not
                // here. Deleting it would destroy the user's file; leaving it
                // silently would report a revert that did not happen.
                report.no_pre_image.push(item.path.clone());
            }
            Some(id) => {
                let content = crate::agent::blobs::get(root, id)
                    .with_context(|| format!("restoring {}", item.path))?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("recreating {}", parent.display()))?;
                }
                std::fs::write(&path, &content)
                    .with_context(|| format!("writing {}", path.display()))?;
                report.restored.push(item.path.clone());
            }
            None => {
                // created inside the window: reverting means it should not exist
                match std::fs::remove_file(&path) {
                    Ok(()) => report.deleted.push(item.path.clone()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        return Err(e).with_context(|| format!("removing {}", path.display()));
                    }
                }
            }
        }
    }
    Ok(report)
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
    restore_paths_in(root, ShadowStore::Local, sha, targets)
}

pub fn restore_paths_in(
    root: &Path,
    store: ShadowStore,
    sha: &str,
    targets: &[Target],
) -> Result<RestoreReport> {
    let Some(shadow) = shadow(root, store).or_else(|| shadow(root, ShadowStore::Local)) else {
        anyhow::bail!("no shadow repository: git is unavailable or [undo].shadow is off");
    };
    let mut report = RestoreReport::default();
    for target in targets {
        let absolute = root.join(&target.path);
        let live = current_hash(&absolute);

        // Only skip when the host knows what it left behind AND the file is
        // still there: a file the agent deleted has no hash to compare.
        if let (Some(expected), Some(live)) = (&target.agent_hash, &live)
            && expected != live
        {
            report.skipped.push(target.path.clone());
            continue;
        }

        match shadow.show(sha, &target.path)? {
            Some(content) => {
                if let Some(parent) = absolute.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                // bytes as recorded, no git filters (§2.5: core.autocrlf = false)
                std::fs::write(&absolute, &content)
                    .with_context(|| format!("restoring {}", target.path))?;
                report.restored.push(target.path.clone());
            }
            None => {
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

    /// §2.5: blobs are *"purged together with the session journal"*. A chain
    /// whose journal is gone has nothing left that could reference its
    /// checkpoints; a chain whose journal is still there must survive, and so
    /// must the session running the maintenance.
    #[test]
    fn maintenance_drops_only_the_chains_whose_journal_is_gone() {
        let dir = tempfile::Builder::new()
            .prefix("sqwai-maintain")
            .tempdir()
            .unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), b"fn main() {}").unwrap();
        let cfg = crate::config::UndoConfig::default();

        let Some(shadow) = shadow(root, cfg.shadow) else {
            // no git binary: layer 2 is absent and there is nothing to test
            return;
        };
        for session in ["current", "alive", "finished"] {
            fs::write(root.join("a.rs"), format!("// {session}")).unwrap();
            shadow.snapshot(session, "one").unwrap();
        }
        // two of the three still have a journal on disk
        fs::create_dir_all(root.join(".sqwai/journal")).unwrap();
        for session in ["current", "alive"] {
            fs::write(root.join(format!(".sqwai/journal/{session}.jsonl")), b"").unwrap();
        }

        let report = maintain(root, &cfg, "current").unwrap();
        assert_eq!(report.chains_dropped, vec!["finished".to_string()]);
        assert!(
            report.collected,
            "dropping a chain must be followed by a gc"
        );

        let left = shadow.sessions().unwrap();
        assert!(left.contains(&"current".to_string()));
        assert!(left.contains(&"alive".to_string()));
        assert!(!left.contains(&"finished".to_string()));
    }

    /// `keep_per_session` is reported, not enforced — truncating a commit
    /// chain would rewrite the shas the journal recorded. The report has to
    /// say so, or the setting silently means nothing.
    #[test]
    fn maintenance_reports_a_chain_over_the_keep_limit() {
        let dir = tempfile::Builder::new()
            .prefix("sqwai-keep")
            .tempdir()
            .unwrap();
        let root = dir.path();
        let cfg = crate::config::UndoConfig {
            keep_per_session: 2,
            ..Default::default()
        };

        let Some(shadow) = shadow(root, cfg.shadow) else {
            return;
        };
        for n in 0..4 {
            fs::write(root.join("a.rs"), format!("// {n}")).unwrap();
            shadow.snapshot("current", "snap").unwrap();
        }
        let report = maintain(root, &cfg, "current").unwrap();
        assert_eq!(report.kept_chain_len, 4);
        assert!(
            report.over_keep_limit,
            "four commits against a limit of two has to be visible"
        );
    }

    /// §2.5's headline promise: undo works in a project that is not a git
    /// repository at all. No `git init` anywhere in this test.
    #[test]
    fn a_project_without_git_can_still_be_reverted() {
        use crate::agent::journal::PreImage;
        let dir = tempfile::Builder::new()
            .prefix("sqwai-blob-undo")
            .tempdir()
            .unwrap();
        let root = dir.path();
        assert!(
            !root.join(".git").exists(),
            "the fixture must not be a repository"
        );

        // a file the agent edited: its pre-image is in the store
        let edited = root.join("src/main.rs");
        fs::create_dir_all(edited.parent().unwrap()).unwrap();
        let original = b"fn main() { println!(\"one\"); }";
        let blob = crate::agent::blobs::put(root, original).unwrap();
        fs::write(&edited, b"fn main() { println!(\"two\"); }").unwrap();

        // a file the agent created: reverting means removing it
        let created = root.join("src/extra.rs");
        fs::write(&created, b"// new").unwrap();

        let report = restore_from_blobs(
            root,
            &[
                PreImage {
                    path: "src/main.rs".into(),
                    blob_before: Some(blob),
                    existed_before: true,
                    agent_hash: None,
                },
                PreImage {
                    path: "src/extra.rs".into(),
                    blob_before: None,
                    existed_before: false,
                    agent_hash: None,
                },
            ],
        )
        .unwrap();

        assert_eq!(fs::read(&edited).unwrap(), original, "byte-for-byte");
        assert!(!created.exists(), "a file created in the window is removed");
        assert_eq!(report.restored, vec!["src/main.rs".to_string()]);
        assert_eq!(report.deleted, vec!["src/extra.rs".to_string()]);
        assert!(report.skipped.is_empty());
    }

    /// The same rule as the git path: a file the user changed after the agent
    /// wrote it is reported, never overwritten.
    #[test]
    fn a_file_edited_outside_sqwai_is_left_alone() {
        use crate::agent::journal::PreImage;
        let dir = tempfile::Builder::new()
            .prefix("sqwai-blob-skip")
            .tempdir()
            .unwrap();
        let root = dir.path();
        let path = root.join("notes.md");

        let original = b"# notes\n";
        let blob = crate::agent::blobs::put(root, original).unwrap();
        // what the host left, recorded in the journal
        fs::write(&path, b"# notes, by the agent\n").unwrap();
        let agent_hash = current_hash(&path);
        // ...and then the user edited it themselves
        fs::write(&path, b"# notes, by me\n").unwrap();

        let report = restore_from_blobs(
            root,
            &[PreImage {
                path: "notes.md".into(),
                blob_before: Some(blob),
                existed_before: true,
                agent_hash,
            }],
        )
        .unwrap();

        assert_eq!(report.skipped, vec!["notes.md".to_string()]);
        assert!(report.restored.is_empty());
        assert_eq!(
            fs::read(&path).unwrap(),
            b"# notes, by me\n",
            "the user's edit survives"
        );
    }

    /// The trap my own first version walked into: a record written before the
    /// blob store existed has no pre-image, and treating that as "the agent
    /// created this file" would delete a file it had merely edited.
    #[test]
    fn a_record_without_a_pre_image_is_reported_not_deleted() {
        use crate::agent::journal::PreImage;
        let dir = tempfile::Builder::new()
            .prefix("sqwai-blob-legacy")
            .tempdir()
            .unwrap();
        let root = dir.path();
        let path = root.join("legacy.rs");
        fs::write(&path, b"// edited by the agent, pre-image never stored").unwrap();

        let report = restore_from_blobs(
            root,
            &[PreImage {
                path: "legacy.rs".into(),
                blob_before: None,
                existed_before: true,
                agent_hash: None,
            }],
        )
        .unwrap();

        assert!(path.exists(), "the file must survive");
        assert_eq!(report.no_pre_image, vec!["legacy.rs".to_string()]);
        assert!(report.deleted.is_empty(), "nothing may be deleted");
        assert!(report.restored.is_empty());
    }

    /// A blob the store never got cannot be silently treated as "nothing to
    /// do": the file would stay modified while undo reported success.
    #[test]
    fn a_missing_blob_fails_loudly() {
        use crate::agent::journal::PreImage;
        let dir = tempfile::Builder::new()
            .prefix("sqwai-blob-missing")
            .tempdir()
            .unwrap();
        let err = restore_from_blobs(
            dir.path(),
            &[PreImage {
                path: "gone.rs".into(),
                blob_before: Some("blake3:0000".into()),
                existed_before: true,
                agent_hash: None,
            }],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("restoring gone.rs"), "{err}");
    }

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

    /// Hash in exactly the format a journal `file_diff` record carries in
    /// `hash_after` (`sha256:`-prefixed). Earlier this helper returned raw
    /// hex while production records are prefixed, so the tests passed and
    /// production skipped everything — every `agent_hash` below must go
    /// through here, never hand-rolled.
    fn hash_of(path: &Path) -> String {
        let mut hasher = Sha256::new();
        hasher.update(std::fs::read(path).unwrap());
        format!("sha256:{:x}", hasher.finalize())
    }

    /// The live hash and the journal hash are compared for equality, so they
    /// must share a format. If either side drops the `sha256:` prefix, every
    /// scoped restore degrades to reporting skips.
    #[test]
    fn live_and_journal_hashes_share_a_format() {
        let repo = tmp_repo();
        let target = repo.path().join("a.txt");
        assert_eq!(current_hash(&target).unwrap(), hash_of(&target));
    }

    /// End to end across the module boundary: a hash that went through a real
    /// journal `file_diff` record must come back out and restore, not skip.
    /// Unit hashes on both sides used to match each other while production
    /// prefixed only the journal side, so every scoped undo skipped every
    /// file and no test saw it.
    #[test]
    fn restore_accepts_hashes_read_back_from_the_journal() {
        use crate::agent::journal::Journal;
        use serde_json::json;

        let repo = tmp_repo();
        let dir = repo.path();
        let sha = snapshot_session(dir, ShadowStore::Local, "test", "pre_mutation")
            .expect("no shadow repo")
            .expect("snapshot");
        std::fs::write(dir.join("a.txt"), "agent edit\n").unwrap();

        let mut journal = Journal::open(dir, "session").unwrap();
        journal
            .append(
                "file_diff",
                json!({
                    "path": "a.txt",
                    "hash_after": hash_of(&dir.join("a.txt")),
                    "checkpoint": sha,
                }),
            )
            .unwrap();
        let targets: Vec<Target> = Journal::recorded_writes(dir, std::slice::from_ref(&sha))
            .unwrap()
            .into_iter()
            .map(|(path, agent_hash)| Target { path, agent_hash })
            .collect();

        let report = restore_paths(dir, &sha, &targets).expect("restore");
        assert_eq!(report.restored, vec!["a.txt".to_string()]);
        assert!(report.skipped.is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "hello\n"
        );
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

        let sha = snapshot_session(dir, ShadowStore::Local, "test", "pre_mutation a.txt")
            .expect("no shadow repo")
            .expect("snapshot");
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
        let sha = snapshot_session(dir, ShadowStore::Local, "test", "pre_mutation")
            .expect("no shadow repo")
            .expect("snapshot");
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
        let sha = snapshot_session(dir, ShadowStore::Local, "test", "pre_mutation a.txt")
            .expect("no shadow repo")
            .expect("snapshot");
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
        let sha = snapshot_session(dir, ShadowStore::Local, "test", "pre_bash")
            .expect("no shadow repo")
            .expect("snapshot");
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

        let sha = snapshot_session(dir, ShadowStore::Local, "test", "test")
            .expect("no shadow repo")
            .expect("snapshot");

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
    /// This used to assert that a snapshot needs the user's repository. It
    /// does not any more, and that is the change #17 asked for: the shadow
    /// repository is ours, so layer 2 works in a plain directory too. What it
    /// still needs is the `git` binary.
    fn available_wherever_git_is_installed() {
        let repo = tmp_repo();
        let dir = repo.path();
        assert!(available(dir));
        let plain = dir.join("nested");
        fs::create_dir_all(&plain).unwrap();
        assert_eq!(
            available(&plain),
            crate::agent::shadow::git_available(),
            "a plain directory is as snapshot-able as any other"
        );
        // and asking must not have created anything
        assert!(
            !plain.join(".sqwai").exists(),
            "an availability check initialised a shadow repository"
        );
    }

    // Windows-only: on POSIX a backslash is a valid filename character,
    // so `src\foo.rs` names a different file there and this scenario cannot
    // occur. The normalization itself is covered cross-platform by
    // `show_and_diff_handle_windows_backslash_paths`.
    #[cfg(windows)]
    #[test]
    fn restore_paths_handles_windows_backslash_targets() {
        let repo = tmp_repo();
        let dir = repo.path();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("foo.rs"), "original\n").unwrap();
        let sha = snapshot_session(dir, ShadowStore::Local, "test", "pre_mutation")
            .expect("no shadow repo")
            .expect("snapshot");
        std::fs::write(dir.join("src").join("foo.rs"), "agent modified\n").unwrap();

        let targets = vec![Target {
            path: "src\\foo.rs".into(),
            agent_hash: None,
        }];
        let report = restore_paths(dir, &sha, &targets).expect("restore");
        assert_eq!(report.restored, vec!["src\\foo.rs".to_string()]);
        assert_eq!(report.deleted, Vec::<String>::new());
        assert_eq!(
            std::fs::read_to_string(dir.join("src").join("foo.rs")).unwrap(),
            "original\n"
        );
    }

    /// #196: two sessions snapshotting interleaved work must not interleave
    /// their checkpoint history — each chain holds only its own commits, so
    /// walking one session's chain for undo never sees another's.
    #[test]
    fn session_chains_do_not_interleave() {
        let repo = tmp_repo();
        let dir = repo.path();
        let snap = |session: &str, label: &str, content: &str| {
            std::fs::write(dir.join("a.txt"), content).unwrap();
            snapshot_session(dir, ShadowStore::Local, session, label)
                .expect("no shadow repo")
                .expect("snapshot")
        };
        snap("aaa", "aaa-1", "a1\n");
        snap("bbb", "bbb-1", "b1\n");
        snap("aaa", "aaa-2", "a2\n");

        let shadow = shadow_repo(dir, ShadowStore::Local).expect("shadow repo");
        let labels = |session: &str| {
            shadow
                .commit_log(session)
                .expect("log")
                .iter()
                .map(|(_, label)| label.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(labels("aaa"), vec!["aaa-2".to_string(), "aaa-1".to_string()]);
        assert_eq!(labels("bbb"), vec!["bbb-1".to_string()]);
    }
}
