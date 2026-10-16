//! Checkpoints taken before mutating actions, across two layers.
//!
//! Layer 1 ([`crate::agent::blobs`]) keeps the pre-image of every file the
//! host is about to write, needs no git, and powers `/undo step N`.
//!
//! Layer 2 ([`crate::agent::shadow`]) is the tree snapshot `bash` needs,
//! taken in a shadow repository of our own. It used to be taken as a dangling
//! commit in the *user's* repository, which forbids and which a routine
//! `git gc` in their project silently invalidated (#17). Nothing here touches
//! their `.git` any more, and `git2` is gone from the dependency list per
//! — git is a binary invoked with an explicit `--git-dir`.
//!
//! Restoring is path-scoped: the caller passes the paths the host recorded as
//! its own writes, each with the hash the agent left it at. A path whose
//! content no longer matches that hash was changed outside sqwai and is left
//! alone. Undo must not be able to discard work it did not do.
//!
//! Every function takes the project root explicitly — the agent works on the
//! project directory, never on whatever cwd the process happens to have.

use anyhow::Result;
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
#[cfg(test)] // the Local shorthand the tests call; every product site passes its store to `available_in`
pub fn available(root: &Path) -> bool {
    available_in(root, ShadowStore::Local)
}

/// Why layer 2 is down, in words an undo toast can show — `None` when it is up.
/// Layer 2 fails for exactly three reasons, and naming one of them without
/// looking is how a toast blamed git for a setting the user had switched off.
pub fn unavailable_reason(root: &Path, store: ShadowStore) -> Option<&'static str> {
    if matches!(store, ShadowStore::Off) {
        return Some("shadow off");
    }
    if !crate::agent::shadow::git_available() {
        return Some("no git");
    }
    // the store is on and git answers, so the only way left is that the place
    // the snapshots would live in could not be named
    crate::agent::shadow::dir_for(root, store)
        .is_none()
        .then_some("no checkpoint dir")
}

/// Availability in the configured store: with `[undo].shadow="user"` the
/// snapshots live outside the project, and the Local check would report
/// undo unavailable while it works fine.
pub fn available_in(root: &Path, store: ShadowStore) -> bool {
    unavailable_reason(root, store).is_none()
}

/// Snapshot the worktree onto this session's chain in the shadow repository
///, returning the commit.
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

/// Obtain a checkpoint id before a known-file mutation (`write`, `edit`,
/// `multi_edit`, `patch`):
/// 1. When Layer 2 (`Shadow`) is active, snapshots the worktree. If the
///    worktree was already identical to `head_of(session_id)` (e.g. right
///    after a `step_N_start` / `step_N_finish` boundary commit), returns that
///    `head` SHA instead of `None` so the mutation's `file_diff` is still
///    bound to a valid checkpoint.
/// 2. When Layer 2 is off or `git` is unavailable, mints a deterministic
///    Layer-1 checkpoint id (`blob:<uuid>`) so `session.checkpoints` and
///    `/undo` work seamlessly from the Layer-1 blob store alone.
pub fn checkpoint_for_mutation(
    root: &Path,
    store: ShadowStore,
    session_id: &str,
    label: &str,
) -> String {
    if let Some(shadow) = shadow(root, store) {
        match shadow.snapshot(session_id, label) {
            Ok(Some(sha)) => return sha,
            Ok(None) => {
                if let Some(head) = shadow.head_of(session_id) {
                    return head;
                }
            }
            Err(_) => {}
        }
    }
    format!("blob:{}", uuid::Uuid::new_v4().simple())
}

/// Capture `FileDiff` records for all paths that a `bash` command modified,
/// created, or deleted relative to its pre-bash shadow snapshot `pre_sha`.
///
/// This brings OpenCode-style `before -> after` attribution to `bash`:
/// - Files modified or deleted by `bash` have their pre-bash bytes stored in
///   the Layer-1 blob store (`blob_before`).
/// - Files modified or created by `bash` record their post-bash `hash_after`,
///   so if the user later edits those files by hand, `/undo` detects the hash
///   mismatch and protects the user's manual work instead of clobbering it.
/// - Files deleted by `bash` record an empty `hash_after` (`""`), which tells
///   restore that the agent itself deleted the file and it should be restored.
pub fn bash_file_diffs(
    root: &Path,
    store: ShadowStore,
    pre_sha: &str,
) -> Vec<crate::agent::tools::FileDiff> {
    let Some(shadow) = shadow(root, store) else {
        return Vec::new();
    };
    let Ok(changed) = shadow.changed_files(pre_sha) else {
        return Vec::new();
    };
    let mut diffs = Vec::new();
    for rel in changed {
        let rel_norm = rel.replace('\\', "/");
        if traversal_path(&rel_norm) || rel_norm.split('/').any(|s| s == ".sqwai") {
            continue;
        }
        let abs = root.join(&rel_norm);
        if abs.is_dir() {
            continue;
        }
        let before = shadow.show(pre_sha, &rel_norm).ok().flatten();
        let after = std::fs::read(&abs).ok();
        if before == after {
            continue;
        }
        let blob_before = before
            .as_deref()
            .and_then(|b| crate::agent::blobs::put(root, b).ok());
        let blob_after = after
            .as_deref()
            .and_then(|b| crate::agent::blobs::put(root, b).ok());
        let hash_before = before.as_deref().map(bytes_hash);
        let hash_after = after
            .as_deref()
            .map(bytes_hash)
            .unwrap_or_default();
        diffs.push(crate::agent::tools::FileDiff {
            path: rel_norm,
            added: 0,
            removed: 0,
            hash_before,
            hash_after,
            mode: "bash".to_string(),
            checkpoint: Some(pre_sha.to_string()),
            blob_before,
            blob_after,
        });
    }
    diffs
}

/// Hash-gate for pre-bash snapshots: true when the worktree moved
/// since this chain's last snapshot, or when that cannot be determined.
/// Lets safe-classified commands skip the staging cost of `snapshot_session`
/// while keeping silently-mutating ones (formatters, checkouts) insured.
pub fn tree_changed(root: &Path, store: ShadowStore, session_id: &str) -> bool {
    let Some(shadow) = shadow(root, store) else {
        return true;
    };
    shadow.tree_changed(session_id)
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

pub fn changed_files(root: &Path, store: ShadowStore, sha: &str) -> Result<Vec<String>> {
    let Some(shadow) = shadow(root, store) else {
        anyhow::bail!("no shadow repository");
    };
    shadow.changed_files(sha)
}

/// Worktree paths that differ from the session's shadow head. Bash-written
/// bytes leave no per-file diff of their own (audit H4); this attributes
/// them after the fact so passed receipts covering those paths go stale.
/// Empty when the shadow is off, the chain has no commit, or the tree is
/// clean against the head.
#[cfg(test)] // attribution test only; the undo path reads the diff through restore_paths_in
pub fn changed_since_head(root: &Path, store: ShadowStore, session_id: &str) -> Vec<String> {
    let Some(shadow) = shadow(root, store) else {
        return Vec::new();
    };
    if !shadow.tree_changed(session_id) {
        return Vec::new();
    }
    let Some(head) = shadow
        .commit_log(session_id)
        .ok()
        .and_then(|log| log.into_iter().next())
        .map(|(sha, _)| sha)
    else {
        return Vec::new();
    };
    shadow.changed_files(&head).unwrap_or_default()
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

/// Retention for both layers, run when a session ends.
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
/// deliberate gap rather than an oversight. stores checkpoints as a
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
    /// The loop stopped here: everything listed above was already applied, and
    /// the caller must say so instead of reporting a clean failure. Restore is
    /// not atomic — a read-only blob store or a locked file on Windows is
    /// enough to hit this mid-window.
    pub failed: Option<String>,
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
    Some(bytes_hash(&bytes))
}

fn bytes_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{:x}", hasher.finalize())
}

/// Snapshot the pre-undo state of `paths` into the Layer-1 blob store right
/// before `/undo` modifies the worktree. Paired with [`finalize_redo_images`]
/// after the restore completes so `/redo` knows both what bytes to restore
/// (`blob_before`) and what hash `/undo` left on disk (`agent_hash`).
pub fn capture_pre_undo_blobs(
    root: &Path,
    paths: &[String],
) -> std::collections::HashMap<String, (Option<String>, bool)> {
    let mut out = std::collections::HashMap::new();
    for rel in paths {
        if out.contains_key(rel) || traversal_path(rel) {
            continue;
        }
        let abs = root.join(rel);
        match std::fs::read(&abs) {
            Ok(bytes) => {
                let blob = crate::agent::blobs::put(root, &bytes).ok();
                out.insert(rel.clone(), (blob, true));
            }
            Err(_) => {
                out.insert(rel.clone(), (None, false));
            }
        }
    }
    out
}

/// Build the `PreImage` list for a [`crate::session::RedoEntry`] after `/undo`
/// touched `touched_paths`. Each entry carries the pre-undo blob to restore
/// on `/redo` and the post-undo `agent_hash` currently on disk so `/redo`
/// refuses to clobber a file the user edited after running `/undo`.
pub fn finalize_redo_images(
    root: &Path,
    touched_paths: &[String],
    pre_undo: &std::collections::HashMap<String, (Option<String>, bool)>,
) -> Vec<crate::agent::journal::PreImage> {
    let mut out = Vec::with_capacity(touched_paths.len());
    for rel in touched_paths {
        let Some((blob_before, existed_before)) = pre_undo.get(rel).cloned() else {
            continue;
        };
        let post_undo_hash = current_hash(&root.join(rel)).unwrap_or_default();
        out.push(crate::agent::journal::PreImage {
            path: rel.clone(),
            blob_before,
            existed_before,
            agent_hash: Some(post_undo_hash),
        });
    }
    out
}

/// Restore files from the layer-1 blob store, with no git involved at all.
///
/// This is what means by "the constraint 'undo unavailable outside git
/// repositories' is lifted": every file the host wrote through
/// `write | edit | multi_edit | patch` has its pre-image in the store, keyed
/// from the journal, so putting it back is a copy.
///
/// The same outside-edit rule as the git path applies: a file whose current
/// content is not what the host last left is reported as skipped, never
/// overwritten. A pre-image of `None` means the file did not exist before the
/// undone window, so reverting it means removing it.
/// Journal-provided paths are trusted to be project-relative. A `..`
/// segment or an absolute spelling means the journal is corrupt or
/// tampered; restore must never join it onto the root (audit H17) — it
/// would write or delete outside the project.
fn traversal_path(p: &str) -> bool {
    let path = Path::new(p);
    path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        || p.replace('\\', "/").starts_with('/')
}

pub fn restore_from_blobs(
    root: &Path,
    pre_images: &[crate::agent::journal::PreImage],
) -> Result<RestoreReport> {
    let mut report = RestoreReport::default();
    for item in pre_images {
        if traversal_path(&item.path) {
            report.skipped.push(item.path.clone());
            continue;
        }
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
        // Absent file the agent left content behind on: someone else deleted
        // it after the agent wrote it. Resurrecting the blob would destroy
        // the user's own action — report, do not restore. (Absent with no
        // non-empty agent hash means the agent itself deleted it: that
        // restores below.)
        if current.is_none() && item.agent_hash.as_deref().is_some_and(|h| !h.is_empty()) {
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
                let Ok(content) = crate::agent::blobs::get(root, id) else {
                    report.failed = Some(format!("{}: pre-image unreadable", item.path));
                    break;
                };
                if let Some(parent) = path.parent()
                    && let Err(e) = std::fs::create_dir_all(parent)
                {
                    report.failed = Some(format!("{}: {e}", parent.display()));
                    break;
                }
                if let Err(e) = std::fs::write(&path, &content) {
                    report.failed = Some(format!("{}: {e}", path.display()));
                    break;
                }
                report.restored.push(item.path.clone());
            }
            None => {
                // created inside the window: reverting means it should not exist
                match std::fs::remove_file(&path) {
                    Ok(()) => {
                        report.deleted.push(item.path.clone());
                        prune_empty_dirs(root, &path);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        report.failed = Some(format!("{}: {e}", path.display()));
                        break;
                    }
                }
            }
        }
    }
    Ok(report)
}

/// Does layer 1 hold what this whole window needs? An item is covered when its
/// pre-image blob is still in the store, or when the file did not exist before
/// the window at all (a creation — reverting it is a delete, nothing to read
/// back). Retention or a failed blob write can leave a window half covered, and
/// taking the blob path then reports the rest as "no stored pre-image" while the
/// shadow snapshot could have restored every one of them.
pub fn blobs_cover(root: &Path, pre_images: &[crate::agent::journal::PreImage]) -> bool {
    !pre_images.is_empty()
        && pre_images
            .iter()
            .all(|item| match item.blob_before.as_deref() {
                Some(id) => crate::agent::blobs::has(root, id),
                None => !item.existed_before,
            })
}

/// Remove the directories an undo just emptied, walking up from `path` and
/// never past the project root. A revert that deletes `src/foo/bar.rs` should
/// not leave `src/foo/` standing: the agent created it, so the state to return
/// to has no such directory. Anything non-empty, already gone, or outside the
/// root is left alone — this is cleanup, not a second sweep.
fn prune_empty_dirs(root: &Path, path: &Path) {
    let mut dir = path.parent();
    while let Some(current) = dir {
        if current == root || !current.starts_with(root) {
            break;
        }
        if std::fs::remove_dir(current).is_err() {
            break;
        }
        dir = current.parent();
    }
}

/// Restore exactly `targets` from a snapshot, and nothing else.
///
/// A target present in the snapshot is written back byte for byte; one that is
/// absent from it appeared afterwards and is deleted. A target whose current
/// content does not match `agent_hash` was edited outside sqwai after the agent
/// wrote it, and is reported as skipped rather than overwritten.
///
/// The index and HEAD are never touched, and no path outside `targets` is read
/// or written.
#[cfg(test)] // the Local shorthand the restore tests call; the product uses restore_paths_in
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
        if traversal_path(&target.path) {
            report.skipped.push(target.path.clone());
            continue;
        }
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
        // Absent file the agent left content behind on: the user deleted it
        // after the agent wrote it — resurrecting would destroy their action.
        // (Absent with no non-empty agent hash means the agent itself deleted
        // it: that restores from the snapshot below.)
        if live.is_none() && target.agent_hash.as_deref().is_some_and(|h| !h.is_empty()) {
            report.skipped.push(target.path.clone());
            continue;
        }

        match shadow.show(sha, &target.path) {
            Ok(Some(content)) => {
                if let Some(parent) = absolute.parent()
                    && let Err(e) = std::fs::create_dir_all(parent)
                {
                    report.failed = Some(format!("{}: {e}", parent.display()));
                    break;
                }
                // bytes as recorded, no git filters
                if let Err(e) = std::fs::write(&absolute, &content) {
                    report.failed = Some(format!("{}: {e}", absolute.display()));
                    break;
                }
                report.restored.push(target.path.clone());
            }
            Ok(None) => {
                // not in the snapshot: it appeared after, so undo removes it
                if absolute.exists() {
                    if let Err(e) = std::fs::remove_file(&absolute) {
                        report.failed = Some(format!("{}: {e}", absolute.display()));
                        break;
                    }
                    prune_empty_dirs(root, &absolute);
                }
                report.deleted.push(target.path.clone());
            }
            Err(e) => {
                report.failed = Some(format!("{}: {e:#}", target.path));
                break;
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests;
