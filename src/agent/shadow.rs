//! Layer 2 of §2.5: a shadow git repository, wrapped only around `bash`.
//!
//! Layer 1 covers every mutation whose target is known in advance. `bash` is
//! the one case that cannot be predicted, so it needs a tree snapshot — and
//! §2.5 is explicit about where that snapshot must *not* go:
//!
//! > Neither layer ever touches the user's `.git`: no `index.lock`, no refs,
//! > and no interference with the user's gc/hooks/worktree.
//!
//! Before this module, snapshots were dangling commits in the user's own
//! object database, which meant a routine `git gc` in their repository
//! silently invalidated every checkpoint of the session (#17). Here the
//! snapshots live in a `GIT_DIR` of our own with `core.worktree` pointed at
//! the project, so the user's index, refs, hooks and gc never see them, and
//! the whole thing works in a project that has no repository at all.
//!
//! Git is invoked as a binary, per §5.10 — `git2`/libgit2 is not a dependency
//! and must not be reintroduced.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::ShadowStore;

/// Everything needed to talk to one project's shadow repository.
pub struct Shadow {
    git_dir: PathBuf,
    root: PathBuf,
}

/// Where the shadow repository lives for this project and setting. `None`
/// means layer 2 is off, and layer 1 keeps working — that is the point of
/// having two layers rather than one.
pub fn dir_for(root: &Path, store: ShadowStore) -> Option<PathBuf> {
    match store {
        ShadowStore::Off => None,
        ShadowStore::Local => Some(root.join(".sqwai").join("checkpoints").join("git")),
        ShadowStore::User => {
            // Keyed by the project's path so two checkouts of the same
            // repository do not share a chain. Truncated because the
            // directory name is for humans reading `ls`, not for uniqueness
            // beyond collision resistance.
            let digest = blake3::hash(root.to_string_lossy().as_bytes()).to_hex();
            let key = &digest[..16];
            crate::config::data_dir()
                .ok()
                .map(|dir| dir.join("checkpoints").join(key).join("git"))
        }
    }
}

/// True when the `git` binary answers at all. Layer 2 needs it; layer 1 does
/// not, and §2.5's degradation table says so explicitly.
pub fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

impl Shadow {
    pub fn new(root: &Path, git_dir: PathBuf) -> Self {
        Self {
            git_dir,
            root: root.to_path_buf(),
        }
    }

    /// Open the shadow repository for this project, creating it if needed.
    pub fn open(root: &Path, store: ShadowStore) -> Result<Option<Self>> {
        let Some(git_dir) = dir_for(root, store) else {
            return Ok(None);
        };
        if !git_available() {
            return Ok(None);
        }
        let shadow = Self::new(root, git_dir);
        shadow.ensure()?;
        Ok(Some(shadow))
    }

    /// One git invocation against the shadow repository.
    ///
    /// `--git-dir` and `--work-tree` are passed per call instead of relying on
    /// environment variables, so nothing leaks into a `git` the user or a tool
    /// runs afterwards. The environment is also stripped of the variables that
    /// would redirect us into *their* repository.
    fn git(&self, args: &[&str]) -> Result<String> {
        let out = Command::new("git")
            .arg(format!("--git-dir={}", self.git_dir.display()))
            .arg(format!("--work-tree={}", self.root.display()))
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            // a global `init.templateDir`, hooks path or signing setup must not
            // decide what our snapshots look like
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.git_dir.join("absent.gitconfig"))
            .env("GIT_TERMINAL_PROMPT", "0")
            .current_dir(&self.root)
            .output()
            .with_context(|| format!("running git {}", args.join(" ")))?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Create the repository and write the configuration §2.5 prescribes.
    /// Idempotent: an existing shadow is only reconfigured, never re-created.
    pub fn ensure(&self) -> Result<()> {
        if !self.git_dir.join("HEAD").exists() {
            std::fs::create_dir_all(&self.git_dir).context("creating the shadow git dir")?;
            let out = Command::new("git")
                .arg("init")
                .arg("--bare")
                .arg("--quiet")
                .arg(&self.git_dir)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", self.git_dir.join("absent.gitconfig"))
                // `init.templateDir` in a user's global config can install
                // hooks into every repository it creates, including this one
                .env("GIT_TEMPLATE_DIR", "")
                .output()
                .context("initialising the shadow repository")?;
            if !out.status.success() {
                bail!(
                    "git init failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
        }
        for (key, value) in [
            ("core.worktree", self.root.to_string_lossy().into_owned()),
            // snapshots must preserve raw bytes: a line-ending rewrite would
            // make a restore differ from what was snapshotted
            ("core.autocrlf", "false".into()),
            ("core.symlinks", "true".into()),
            ("core.longpaths", "true".into()),
            ("core.untrackedCache", "true".into()),
            // the user's fsmonitor daemon has no business watching for us
            ("core.fsmonitor", "false".into()),
            ("core.bare", "false".into()),
            // never sign or hook a checkpoint
            ("commit.gpgsign", "false".into()),
            ("core.hooksPath", "/dev/null".into()),
        ] {
            self.git(&["config", key, &value])?;
        }
        self.write_exclude()?;
        Ok(())
    }

    /// `info/exclude` for the shadow index: the project's own ignore rules,
    /// plus what must never be snapshotted regardless of them.
    fn write_exclude(&self) -> Result<()> {
        let mut lines = String::from(
            "# generated by sqwai (§2.5): the project's ignore rules, plus\n\
             # state that must never enter a checkpoint\n\
             .sqwai/\n\
             .git/\n\
             *.lfs\n",
        );
        // Nested repositories (submodules, vendored checkouts) are excluded
        // rather than snapshotted: git would record them as gitlinks whose
        // objects we do not have, and a restore could not reproduce them.
        for nested in nested_git_dirs(&self.root) {
            lines.push_str(&format!("{nested}\n"));
        }
        if let Ok(project) = std::fs::read_to_string(self.root.join(".gitignore")) {
            lines.push_str("\n# from .gitignore\n");
            lines.push_str(&project);
        }
        let info = self.git_dir.join("info");
        std::fs::create_dir_all(&info).context("creating the shadow info directory")?;
        std::fs::write(info.join("exclude"), lines).context("writing the shadow exclude file")?;
        Ok(())
    }

    /// Stage the whole worktree. This is what makes untracked files part of a
    /// snapshot, and it touches only the shadow index.
    fn stage_all(&self) -> Result<()> {
        self.git(&["add", "-A", "--", "."]).map(|_| ())
    }

    pub(crate) fn head_of(&self, session_id: &str) -> Option<String> {
        self.git(&["rev-parse", &session_ref(session_id)])
            .ok()
            .map(|sha| sha.trim().to_string())
            .filter(|sha| !sha.is_empty())
    }

    /// Snapshot the worktree onto this session's chain, returning the commit.
    ///
    /// `Ok(None)` means the tree is identical to the previous snapshot, so no
    /// commit was made — §2.5 asks for exactly that check rather than a commit
    /// per action.
    pub fn snapshot(&self, session_id: &str, label: &str) -> Result<Option<String>> {
        self.stage_all()?;
        let tree = self.git(&["write-tree"])?.trim().to_string();
        let parent = self.head_of(session_id);
        if let Some(parent) = &parent {
            let parent_tree = self.git(&["rev-parse", &format!("{parent}^{{tree}}")])?;
            if parent_tree.trim() == tree {
                return Ok(None);
            }
        }
        let mut args: Vec<String> = vec!["commit-tree".into(), tree, "-m".into(), label.into()];
        if let Some(parent) = parent {
            args.push("-p".into());
            args.push(parent);
        }
        // identity is passed per invocation: the user's own name and email are
        // theirs, and a missing global identity must not make snapshots fail
        let mut command: Vec<&str> =
            vec!["-c", "user.name=sqwai", "-c", "user.email=sqwai@localhost"];
        let owned: Vec<&str> = args.iter().map(String::as_str).collect();
        command.extend(owned);
        let commit = self.git(&command)?.trim().to_string();
        self.git(&["update-ref", &session_ref(session_id), &commit])?;
        Ok(Some(commit))
    }

    /// Snapshot the worktree even if unchanged from parent. Used for boundary checkpoints.
    pub fn snapshot_forced(&self, session_id: &str, label: &str) -> Result<String> {
        self.stage_all()?;
        let tree = self.git(&["write-tree"])?.trim().to_string();
        let parent = self.head_of(session_id);
        let mut args: Vec<String> = vec!["commit-tree".into(), tree, "-m".into(), label.into()];
        if let Some(parent) = parent {
            args.push("-p".into());
            args.push(parent);
        }
        let mut command: Vec<&str> =
            vec!["-c", "user.name=sqwai", "-c", "user.email=sqwai@localhost"];
        let owned: Vec<&str> = args.iter().map(String::as_str).collect();
        command.extend(owned);
        let commit = self.git(&command)?.trim().to_string();
        self.git(&["update-ref", &session_ref(session_id), &commit])?;
        Ok(commit)
    }

    /// Paths that differ between `sha` and the worktree, staged files
    /// included. Used when the host cannot enumerate what a `bash` command
    /// touched.
    pub fn changed_files(&self, sha: &str) -> Result<Vec<String>> {
        self.stage_all()?;
        let out = self.git(&["diff", "--cached", "--name-only", sha, "--"])?;
        Ok(out
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Contents of one path in a snapshot, or `None` when the snapshot does
    /// not contain it — which is how a file created afterwards is recognised.
    pub fn show(&self, sha: &str, path: &str) -> Result<Option<Vec<u8>>> {
        let out = Command::new("git")
            .arg(format!("--git-dir={}", self.git_dir.display()))
            .arg(format!("--work-tree={}", self.root.display()))
            .args(["show", &format!("{sha}:{path}")])
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.git_dir.join("absent.gitconfig"))
            .env("GIT_TERMINAL_PROMPT", "0")
            .current_dir(&self.root)
            .output()
            .context("reading a file from a snapshot")?;
        if out.status.success() {
            return Ok(Some(out.stdout));
        }
        let err = String::from_utf8_lossy(&out.stderr);
        // "exists on disk, but not in" / "does not exist in" — the file simply
        // postdates the snapshot, which is not a failure
        if err.contains("does not exist") || err.contains("exists on disk") {
            return Ok(None);
        }
        bail!("git show {sha}:{path} failed: {}", err.trim());
    }

    /// List commits on a session's chain in reverse chronological order: (sha, label).
    pub fn commit_log(&self, session_id: &str) -> Result<Vec<(String, String)>> {
        let out = match self.git(&["log", "--format=%H\t%s", &session_ref(session_id)]) {
            Ok(out) => out,
            Err(_) => return Ok(Vec::new()),
        };
        Ok(out
            .lines()
            .filter_map(|line| {
                let (sha, label) = line.split_once('\t')?;
                Some((sha.trim().to_string(), label.trim().to_string()))
            })
            .collect())
    }

    /// Unified diff between two snapshots, optionally limited to one path.
    pub fn diff(&self, sha1: &str, sha2: &str, path: Option<&str>) -> Result<String> {
        let mut args = vec!["diff", sha1, sha2];
        if let Some(p) = path {
            args.push("--");
            args.push(p);
        }
        self.git(&args)
    }
}

impl Shadow {
    /// Session chains present in the shadow repository.
    pub fn sessions(&self) -> Result<Vec<String>> {
        let out = self.git(&["for-each-ref", "--format=%(refname)", "refs/sessions/"])?;
        Ok(out
            .lines()
            .filter_map(|line| line.trim().strip_prefix("refs/sessions/"))
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Drop one session's chain. The commits become unreachable, which is what
    /// makes them collectable — nothing is freed until [`Shadow::gc`] runs.
    pub fn drop_session(&self, session_id: &str) -> Result<()> {
        self.git(&["update-ref", "-d", &session_ref(session_id)])
            .map(|_| ())
    }

    /// How many commits one session's chain holds.
    pub fn chain_len(&self, session_id: &str) -> Result<usize> {
        match self.git(&["rev-list", "--count", &session_ref(session_id)]) {
            Ok(out) => Ok(out.trim().parse().unwrap_or(0)),
            // no such ref: an empty chain, not a failure
            Err(_) => Ok(0),
        }
    }

    /// Collect what no session ref reaches any more.
    ///
    /// `--prune=now` because the default two-week grace would keep every
    /// dropped session alive far longer than any of our retention windows.
    pub fn gc(&self) -> Result<()> {
        self.git(&["gc", "--prune=now", "--quiet"]).map(|_| ())
    }

    /// Size of the shadow repository on disk, for `[undo].shadow_max_bytes`.
    pub fn size_bytes(&self) -> u64 {
        dir_size(&self.git_dir)
    }
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(meta) if meta.is_dir() => dir_size(&entry.path()),
            Ok(meta) => meta.len(),
            Err(_) => 0,
        })
        .sum()
}

fn session_ref(session_id: &str) -> String {
    format!("refs/sessions/{session_id}")
}

/// Directories inside the project that are repositories of their own,
/// relative to the root and ready for `info/exclude`.
fn nested_git_dirs(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" || name == ".sqwai" || name == "target" || name == "node_modules" {
            continue;
        }
        if path.join(".git").exists() {
            found.push(format!("{name}/"));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix("sqwai-shadow")
            .tempdir()
            .unwrap();
        std::fs::write(dir.path().join("a.rs"), b"fn main() {}").unwrap();
        dir
    }

    fn open(root: &Path) -> Shadow {
        Shadow::open(root, ShadowStore::Local)
            .unwrap()
            .expect("git is available in the test environment")
    }

    /// §2.5's hard rule, and failure mode 1 of #17: the user's repository must
    /// come out of a snapshot untouched — no refs, no objects, no index.lock.
    #[test]
    fn snapshots_leave_the_users_repository_alone() {
        let dir = project();
        let root = dir.path();
        // give the project a real repository of its own
        let user_git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(root)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("HOME", root)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        user_git(&["init", "--quiet", "-b", "main"]);
        user_git(&["add", "-A"]);
        user_git(&[
            "-c",
            "user.name=u",
            "-c",
            "user.email=u@e",
            "commit",
            "--quiet",
            "-m",
            "initial",
        ]);
        let head_before = user_git(&["rev-parse", "HEAD"]);
        let refs_before = user_git(&["for-each-ref"]);

        let shadow = open(root);
        std::fs::write(root.join("a.rs"), b"fn main() { changed(); }").unwrap();
        let sha = shadow
            .snapshot("session-1", "pre_bash")
            .unwrap()
            .expect("a changed tree produces a commit");

        assert_eq!(user_git(&["rev-parse", "HEAD"]), head_before, "HEAD moved");
        assert_eq!(user_git(&["for-each-ref"]), refs_before, "refs changed");
        assert!(
            !root.join(".git/index.lock").exists(),
            "left an index.lock behind"
        );
        // the user's own status is unaffected: their index still holds the
        // original commit's content, not ours
        assert!(
            user_git(&["status", "--porcelain"]).contains("a.rs"),
            "the user's view of their own worktree changed"
        );
        // and the snapshot is ours, in our own object database
        assert!(
            !user_git(&["cat-file", "-t", &sha]).contains("commit"),
            "our commit ended up in the user's object database"
        );
    }

    /// A project that is not a repository at all still gets layer 2, because
    /// the shadow repository is ours and does not need theirs.
    #[test]
    fn a_project_without_git_still_gets_snapshots() {
        let dir = project();
        let root = dir.path();
        assert!(!root.join(".git").exists());
        let shadow = open(root);
        let first = shadow.snapshot("s", "one").unwrap().unwrap();
        std::fs::write(root.join("b.rs"), b"// new").unwrap();
        let second = shadow.snapshot("s", "two").unwrap().unwrap();
        assert_ne!(first, second);
        assert_eq!(
            shadow.changed_files(&first).unwrap(),
            vec!["b.rs".to_string()],
            "an untracked file is part of the snapshot"
        );
    }

    /// §2.5: "A pre-checkpoint is created only if the tree has changed since
    /// the previous one". Without this a session accumulates one commit per
    /// action, most of them identical.
    #[test]
    fn an_unchanged_tree_produces_no_second_commit() {
        let dir = project();
        let shadow = open(dir.path());
        assert!(shadow.snapshot("s", "first").unwrap().is_some());
        assert_eq!(
            shadow.snapshot("s", "second").unwrap(),
            None,
            "nothing changed, so there is nothing to snapshot"
        );
    }

    #[test]
    fn a_file_created_after_the_snapshot_is_absent_from_it() {
        let dir = project();
        let root = dir.path();
        let shadow = open(root);
        let sha = shadow.snapshot("s", "before").unwrap().unwrap();
        std::fs::write(root.join("later.rs"), b"// later").unwrap();
        assert_eq!(shadow.show(&sha, "later.rs").unwrap(), None);
        assert_eq!(
            shadow.show(&sha, "a.rs").unwrap().unwrap(),
            b"fn main() {}",
            "and an existing file comes back byte for byte"
        );
    }

    /// `.sqwai/` holds the journal, the plan and the blob store. A snapshot
    /// that contained them would grow without bound and a restore could
    /// rewrite the host's own state.
    #[test]
    fn host_state_is_never_snapshotted() {
        let dir = project();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".sqwai/journal")).unwrap();
        std::fs::write(root.join(".sqwai/journal/s.jsonl"), b"{}").unwrap();
        let shadow = open(root);
        let sha = shadow.snapshot("s", "one").unwrap().unwrap();
        assert_eq!(shadow.show(&sha, ".sqwai/journal/s.jsonl").unwrap(), None);
    }

    #[test]
    fn the_shadow_directory_follows_the_setting() {
        let dir = project();
        let root = dir.path();
        assert_eq!(
            dir_for(root, ShadowStore::Local),
            Some(root.join(".sqwai").join("checkpoints").join("git"))
        );
        assert_eq!(dir_for(root, ShadowStore::Off), None);
        let user = dir_for(root, ShadowStore::User).expect("a user data dir");
        assert!(
            user.starts_with(crate::config::data_dir().unwrap()),
            "{user:?}"
        );
        assert!(user.ends_with("git"));
    }
}
