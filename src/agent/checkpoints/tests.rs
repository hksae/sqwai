use super::*;
use std::fs;

/// Audit H4: bash mutations leave no file_diff; the shadow-head diff
/// attributes them so passed receipts over those paths can go stale.
#[test]
fn changed_since_head_attributes_unrecorded_writes() {
    let dir = tempfile::Builder::new()
        .prefix("sqwai-h4")
        .tempdir()
        .unwrap();
    let root = dir.path();
    fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
    if shadow(root, ShadowStore::Local).is_none() {
        return; // no git binary: layer 2 absent
    }
    snapshot_boundary(root, ShadowStore::Local, "s", "base")
        .unwrap()
        .expect("first boundary commits");
    // a bash-style write with no event record anywhere
    fs::write(root.join("a.rs"), "fn a() { changed(); }\n").unwrap();
    let paths = changed_since_head(root, ShadowStore::Local, "s");
    assert!(
        paths.iter().any(|p| p.ends_with("a.rs")),
        "bash-written change must be attributed: {paths:?}"
    );
    // a clean tree attributes nothing
    snapshot_boundary(root, ShadowStore::Local, "s", "after")
        .unwrap()
        .expect("boundary");
    assert!(changed_since_head(root, ShadowStore::Local, "s").is_empty());
}

/// Audit H17: restore joins events-provided paths straight onto the
/// root. A corrupted or hand-edited events with `..` in a path made
/// undo delete (or overwrite) files outside the project.
#[test]
fn restore_refuses_traversal_paths_from_the_event_log() {
    let dir = tempfile::Builder::new()
        .prefix("sqwai-traversal")
        .tempdir()
        .unwrap();
    let root = dir.path().join("project");
    fs::create_dir_all(&root).unwrap();
    let outside = dir.path().join("outside.txt");
    fs::write(&outside, "user data").unwrap();

    // agent-created-inside-the-window shape: revert = delete
    let pre_images = vec![crate::agent::events::PreImage {
        path: "../outside.txt".to_string(),
        blob_before: None,
        existed_before: false,
        agent_hash: None,
    }];
    let report = restore_from_blobs(&root, &pre_images).unwrap();
    assert!(
        outside.exists(),
        "restore followed a traversal path out of the root"
    );
    assert_eq!(fs::read_to_string(&outside).unwrap(), "user data");
    assert!(
        report.skipped.iter().any(|p| p.contains("..")),
        "traversal target must be reported skipped: {:?}",
        report.skipped
    );
}

#[test]
fn available_in_respects_the_configured_store() {
    let missing = std::env::temp_dir().join("sqwai-no-such-project-undo");
    // off means layer 2 is off, wherever the project is
    assert!(!available_in(&missing, ShadowStore::Off));
    // the legacy entry point is the local store
    assert_eq!(
        available_in(&missing, ShadowStore::Local),
        available(&missing)
    );
}

/// blobs are *"purged together with the session events"*. A chain
/// whose events is gone has nothing left that could reference its
/// checkpoints; a chain whose events is still there must survive, and so
/// must the session running the maintenance.
#[test]
fn maintenance_drops_only_the_chains_whose_event_log_is_gone() {
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
    // two of the three still have an event log on disk
    fs::create_dir_all(root.join(".sqwai/events")).unwrap();
    for session in ["current", "alive"] {
        fs::write(root.join(format!(".sqwai/events/{session}.jsonl")), b"").unwrap();
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
/// chain would rewrite the shas the event log recorded. The report has to
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

/// headline promise: undo works in a project that is not a git
/// repository at all. No `git init` anywhere in this test.
#[test]
fn a_project_without_git_can_still_be_reverted() {
    use crate::agent::events::PreImage;
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
    use crate::agent::events::PreImage;
    let dir = tempfile::Builder::new()
        .prefix("sqwai-blob-skip")
        .tempdir()
        .unwrap();
    let root = dir.path();
    let path = root.join("notes.md");

    let original = b"# notes\n";
    let blob = crate::agent::blobs::put(root, original).unwrap();
    // what the host left, recorded in the event log
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

/// Phase 1.4 PoC: the agent edits a file, the USER deletes it, undo runs.
/// Resurrecting the blob would destroy the user's own action — an absent
/// file the agent left content behind on stays absent (reported, not
/// restored). Contrast the agent-deleted case, which must come back.
#[test]
fn a_file_the_user_deleted_stays_deleted() {
    use crate::agent::events::PreImage;
    let dir = tempfile::Builder::new()
        .prefix("sqwai-blob-userdel")
        .tempdir()
        .unwrap();
    let root = dir.path();
    let path = root.join("notes.md");

    let original = b"# notes\n";
    let blob = crate::agent::blobs::put(root, original).unwrap();
    // what the host left, recorded in the event log
    fs::write(&path, b"# notes, by the agent\n").unwrap();
    let agent_hash = current_hash(&path);
    // ...and then the user deleted it themselves
    fs::remove_file(&path).unwrap();

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
    assert!(
        !path.exists(),
        "the user's deletion survives: undo must not resurrect it"
    );
}

/// The trap my own first version walked into: a record written before the
/// blob store existed has no pre-image, and treating that as "the agent
/// created this file" would delete a file it had merely edited.
#[test]
fn a_record_without_a_pre_image_is_reported_not_deleted() {
    use crate::agent::events::PreImage;
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
/// do": the file would stay modified while undo reported success. It also
/// cannot be reported as a clean failure — the items before it were already
/// put back, and the user has to know which files the tree lost.
#[test]
fn a_missing_blob_stops_the_restore_and_reports_what_already_happened() {
    use crate::agent::events::PreImage;
    let dir = tempfile::Builder::new()
        .prefix("sqwai-blob-missing")
        .tempdir()
        .unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("made")).unwrap();
    fs::write(root.join("made/new.rs"), b"agent created this").unwrap();

    let report = restore_from_blobs(
        root,
        &[
            PreImage {
                path: "made/new.rs".into(),
                blob_before: None,
                existed_before: false,
                agent_hash: None,
            },
            PreImage {
                path: "gone.rs".into(),
                blob_before: Some("blake3:0000".into()),
                existed_before: true,
                agent_hash: None,
            },
        ],
    )
    .unwrap();

    assert_eq!(
        report.deleted,
        vec!["made/new.rs".to_string()],
        "the work before the failure must be reported"
    );
    let failure = report.failed.expect("the missing blob stops the loop");
    assert!(failure.contains("gone.rs"), "{failure}");
}

/// The layer choice, nailed down: one surviving blob must not be read as
/// "layer 1 covers this window", or the other files get reported as
/// unrevertible while the shadow snapshot had every one of them.
#[test]
fn blobs_cover_requires_the_whole_window_not_one_lucky_file() {
    use crate::agent::events::PreImage;
    let dir = tempfile::Builder::new()
        .prefix("sqwai-cover")
        .tempdir()
        .unwrap();
    let root = dir.path();
    let stored = crate::agent::blobs::put(root, b"old bytes").unwrap();
    let item = |path: &str, blob: Option<String>, existed: bool| PreImage {
        path: path.into(),
        blob_before: blob,
        existed_before: existed,
        agent_hash: None,
    };
    let creation = item("new.rs", None, false);
    let backed = item("a.rs", Some(stored.clone()), true);
    let purged = item("b.rs", Some("blake3:absent".into()), true);
    let legacy = item("c.rs", None, true);

    assert!(
        blobs_cover(root, &[creation.clone(), backed.clone()]),
        "a creation needs no blob"
    );
    assert!(!blobs_cover(root, &[backed.clone(), purged]));
    assert!(!blobs_cover(root, &[backed.clone(), legacy]));
    assert!(!blobs_cover(root, &[]), "an empty window covers nothing");
}

/// Reverting a created file must not leave the directories the agent made
/// for it: the state to return to has no `src/foo/` either.
#[test]
fn undo_removes_the_directories_it_emptied() {
    use crate::agent::events::PreImage;
    let dir = tempfile::Builder::new()
        .prefix("sqwai-blob-dirs")
        .tempdir()
        .unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("deep/inner")).unwrap();
    fs::write(root.join("deep/inner/new.rs"), b"created by the agent").unwrap();
    // a neighbour that must survive: pruning stops at a non-empty directory
    fs::write(root.join("deep/keep.txt"), b"mine").unwrap();

    let report = restore_from_blobs(
        root,
        &[PreImage {
            path: "deep/inner/new.rs".into(),
            blob_before: None,
            existed_before: false,
            agent_hash: None,
        }],
    )
    .unwrap();

    assert_eq!(report.deleted, vec!["deep/inner/new.rs".to_string()]);
    assert!(!root.join("deep/inner").exists(), "emptied dir must go");
    assert!(root.join("deep").exists(), "a non-empty parent stays");
    assert!(root.join("deep/keep.txt").exists());
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

/// Hash in exactly the format an event log `file_diff` record carries in
/// `hash_after` (`sha256:`-prefixed). Earlier this helper returned raw
/// hex while production records are prefixed, so the tests passed and
/// production skipped everything — every `agent_hash` below must go
/// through here, never hand-rolled.
fn hash_of(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(std::fs::read(path).unwrap());
    format!("sha256:{:x}", hasher.finalize())
}

/// The live hash and the event log hash are compared for equality, so they
/// must share a format. If either side drops the `sha256:` prefix, every
/// scoped restore degrades to reporting skips.
#[test]
fn live_and_event_hashes_share_a_format() {
    let repo = tmp_repo();
    let target = repo.path().join("a.txt");
    assert_eq!(current_hash(&target).unwrap(), hash_of(&target));
}

/// End to end across the module boundary: a hash that went through a real
/// events `file_diff` record must come back out and restore, not skip.
/// Unit hashes on both sides used to match each other while production
/// prefixed only the event log side, so every scoped undo skipped every
/// file and no test saw it.
#[test]
fn restore_accepts_hashes_read_back_from_the_event_log() {
    use crate::agent::events::EventLog;
    use serde_json::json;

    let repo = tmp_repo();
    let dir = repo.path();
    let sha = snapshot_session(dir, ShadowStore::Local, "test", "pre_mutation")
        .expect("no shadow repo")
        .expect("snapshot");
    std::fs::write(dir.join("a.txt"), "agent edit\n").unwrap();

    let mut events = EventLog::open(dir, "session").unwrap();
    events
        .append(
            "file_diff",
            json!({
                "path": "a.txt",
                "hash_after": hash_of(&dir.join("a.txt")),
                "checkpoint": sha,
            }),
        )
        .unwrap();
    let targets: Vec<Target> = EventLog::recorded_writes(dir, std::slice::from_ref(&sha))
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
    assert_eq!(
        labels("aaa"),
        vec!["aaa-2".to_string(), "aaa-1".to_string()]
    );
    assert_eq!(labels("bbb"), vec!["bbb-1".to_string()]);
}

#[test]
fn edit_tool_stores_original_content_not_path_in_layer1_blob_and_works_with_shadow_off() {
    use crate::agent::events::EventLog;
    use serde_json::json;

    let dir = tempfile::Builder::new()
        .prefix("sqwai-edit-blob-")
        .tempdir()
        .unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    let file_path = root.join("src/lib.rs");
    let original = "pub fn answer() -> u32 {\n    41\n}\n";
    fs::write(&file_path, original).unwrap();

    let mut ctx = crate::agent::tools::ToolCtx::new(root);
    ctx.session_id = "s-edit".to_string();
    ctx.shadow_store = ShadowStore::Off;
    crate::agent::tools::execute(&mut ctx, "read", &json!({ "file_path": "src/lib.rs" }));
    let out = crate::agent::tools::execute(
        &mut ctx,
        "edit",
        &json!({
            "file_path": "src/lib.rs",
            "old_string": "41",
            "new_string": "42",
        }),
    );

    let diff = out.file_diff.expect("edit must produce FileDiff");
    let ckpt = diff
        .checkpoint
        .clone()
        .expect("checkpoint_for_mutation must return a blob:* id when shadow is off");
    assert!(ckpt.starts_with("blob:"), "unexpected ckpt id: {ckpt}");
    assert_eq!(
        fs::read_to_string(&file_path).unwrap(),
        "pub fn answer() -> u32 {\n    42\n}\n"
    );

    let mut events = EventLog::open(root, "s-edit").unwrap();
    events
        .append(
            "file_diff",
            json!({
                "path": diff.path,
                "hash_before": diff.hash_before,
                "hash_after": diff.hash_after,
                "blob_before": diff.blob_before,
                "blob_after": diff.blob_after,
                "checkpoint": ckpt,
            }),
        )
        .unwrap();

    let pre_images = EventLog::recorded_pre_images(root, std::slice::from_ref(&ckpt)).unwrap();
    assert_eq!(pre_images.len(), 1);
    let report = restore_from_blobs(root, &pre_images).unwrap();
    assert_eq!(report.restored, vec!["src/lib.rs".to_string()]);
    assert_eq!(
        fs::read_to_string(&file_path).unwrap(),
        original,
        "edit pre-image must restore original file content, not the path string"
    );
}

#[test]
fn bash_file_diffs_records_layer1_blobs_and_hashes_for_scoped_undo() {
    use crate::agent::events::EventLog;
    use serde_json::json;

    let repo = tmp_repo();
    let dir = repo.path();
    fs::write(dir.join("keep_delete.txt"), "before bash deleted me\n").unwrap();
    let sha = snapshot_session(dir, ShadowStore::Local, "s-bash", "pre_bash")
        .expect("no shadow repo")
        .expect("snapshot");

    // Simulate bash modifying a.txt, creating b.txt, and deleting keep_delete.txt
    fs::write(dir.join("a.txt"), "modified by bash\n").unwrap();
    fs::write(dir.join("b.txt"), "created by bash\n").unwrap();
    fs::remove_file(dir.join("keep_delete.txt")).unwrap();

    let diffs = bash_file_diffs(dir, ShadowStore::Local, &sha);
    assert_eq!(diffs.len(), 3, "diffs: {diffs:?}");

    let mut events = EventLog::open(dir, "s-bash").unwrap();
    for d in &diffs {
        events
            .append(
                "file_diff",
                json!({
                    "path": d.path,
                    "hash_before": d.hash_before,
                    "hash_after": d.hash_after,
                    "blob_before": d.blob_before,
                    "blob_after": d.blob_after,
                    "checkpoint": d.checkpoint,
                }),
            )
            .unwrap();
    }

    // User manually edits a.txt after bash completed
    fs::write(dir.join("a.txt"), "user edit after bash\n").unwrap();

    let pre_images = EventLog::recorded_pre_images(dir, std::slice::from_ref(&sha)).unwrap();
    assert!(blobs_cover(dir, &pre_images));
    let report = restore_from_blobs(dir, &pre_images).unwrap();

    assert_eq!(
        report.skipped,
        vec!["a.txt".to_string()],
        "user edit after bash must be protected"
    );
    assert_eq!(
        fs::read_to_string(dir.join("a.txt")).unwrap(),
        "user edit after bash\n"
    );
    assert_eq!(report.deleted, vec!["b.txt".to_string()]);
    assert!(!dir.join("b.txt").exists());
    assert_eq!(report.restored, vec!["keep_delete.txt".to_string()]);
    assert_eq!(
        fs::read_to_string(dir.join("keep_delete.txt")).unwrap(),
        "before bash deleted me\n"
    );
}
