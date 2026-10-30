use super::*;

fn temp_project(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sqwai-state-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_project_key_is_stable_and_specific() {
    let a = temp_project("key-a");
    let b = temp_project("key-b");
    assert_eq!(project_key(&a), project_key(&a));
    assert_ne!(project_key(&a), project_key(&b));
    // the same directory spelled with a trailing separator is the same project
    let same = a.join(".");
    assert_eq!(project_key(&a), project_key(&same));
    std::fs::remove_dir_all(&a).ok();
    std::fs::remove_dir_all(&b).ok();
}

#[test]
fn state_lives_outside_the_project() {
    let root = temp_project("outside");
    let state = project_dir(&root);
    assert!(
        !state.starts_with(root.canonicalize().unwrap()),
        "state must not live inside the worktree: {}",
        state.display()
    );
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&state).ok();
}

/// The first run after the move: durable state that used to sit in `.sqwai`
/// follows the project to the data dir, and the pieces that belong to the
/// worktree stay where they are.
#[test]
fn legacy_state_moves_and_worktree_files_stay() {
    let root = temp_project("migrate");
    let legacy = root.join(".sqwai");
    std::fs::create_dir_all(legacy.join("events")).unwrap();
    std::fs::write(legacy.join("events/s1.jsonl"), b"{}\n").unwrap();
    std::fs::create_dir_all(legacy.join("plans")).unwrap();
    std::fs::write(legacy.join("plans/p.json"), b"{}").unwrap();
    std::fs::create_dir_all(legacy.join("checkpoints/blobs/ab")).unwrap();
    std::fs::write(legacy.join("checkpoints/blobs/ab/x"), b"blob").unwrap();
    std::fs::create_dir_all(legacy.join("spill")).unwrap();
    std::fs::write(legacy.join("spill/s.txt"), b"spill").unwrap();
    std::fs::create_dir_all(legacy.join("skills/one")).unwrap();
    std::fs::write(legacy.join("skills/one/SKILL.md"), b"skill").unwrap();
    std::fs::create_dir_all(legacy.join("exports")).unwrap();
    std::fs::write(legacy.join("exports/chat.md"), b"export").unwrap();
    std::fs::create_dir_all(legacy.join("lock")).unwrap();
    std::fs::write(legacy.join("lock/old.lock"), b"pid=1").unwrap();

    let state = project_dir(&root);
    assert!(state.join("events/s1.jsonl").exists(), "event log moved");
    assert!(state.join("plans/p.json").exists(), "plan moved");
    assert!(
        state.join("checkpoints/blobs/ab/x").exists(),
        "checkpoints moved"
    );
    assert!(
        !legacy.join("events").exists()
            && !legacy.join("plans").exists()
            && !legacy.join("checkpoints").exists(),
        "the legacy copies must not stay behind"
    );
    // worktree-owned pieces stay, and a stale lock is never carried over
    assert!(legacy.join("spill/s.txt").exists());
    assert!(legacy.join("skills/one/SKILL.md").exists());
    assert!(legacy.join("exports/chat.md").exists());
    assert!(!state.join("lock/old.lock").exists());
    assert!(legacy.join("lock/old.lock").exists());

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&state).ok();
}
