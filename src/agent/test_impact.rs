//! Test-impact selection for `plan verify` (§2.4.11).
//!
//! When the acceptance check is a bare test-runner invocation, the host
//! runs the tests covering the changed files first instead of the whole
//! suite. The receipt records what actually ran, the outcome says so, and
//! `complete` still runs the full suite — so a green verify means the
//! covering tests passed, while the suite-wide verdict stays with
//! `complete`. Anything unrecognized (other runners, user-selected
//! subsets, no changed files, no covering tests, an over-large impact
//! set) falls back to the authored command unchanged.

use std::path::Path;

use crate::agent::graph::{Direction, GraphStore, NeighborQuery};

/// Beyond this many covering files the subset is the suite in disguise:
/// run the authored command instead (also bounds argv length).
const MAX_IMPACT_FILES: usize = 50;
/// Reverse-import closure caps: depth and nodes. Over the cap the impact
/// set is unbounded for practical purposes — run the full command.
const CLOSURE_DEPTH: u8 = 3;
const CLOSURE_NODE_CAP: usize = 300;
/// `-run` narrowing needs exact test names; past this many, package-only.
const MAX_IMPACT_NAMES: usize = 20;

/// A subset invocation replacing the authored check for this verify run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Impacted {
    /// what to execute (subset runner invocation)
    pub command: String,
    /// covering test files, project-relative, sorted
    pub files: Vec<String>,
    /// one-liner for the outcome (transparency, not diagnostics)
    pub note: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Runner {
    Cargo,
    Pytest,
    Go,
}

/// A recognized bare runner invocation plus its benign flags. Anything
/// the user already narrowed (paths, `-k`, `--test`, `-run`, packages)
/// disqualifies — second-guessing an explicit selection is wrong, and a
/// narrowed command is already fast.
fn split_runner(command: &str) -> Option<(Runner, Vec<String>)> {
    let words: Vec<&str> = command.split_whitespace().collect();
    // leading VAR= assignments are environment, not selection
    let mut start = 0;
    while start < words.len()
        && words[start].contains('=')
        && !words[start].starts_with('-')
    {
        start += 1;
    }
    let words = &words[start..];
    let (runner, rest) = match words {
        ["cargo", "test", rest @ ..] => (Runner::Cargo, rest),
        ["pytest", rest @ ..] => (Runner::Pytest, rest),
        ["go", "test", rest @ ..] => (Runner::Go, rest),
        _ => return None,
    };
    let mut flags = Vec::new();
    for token in rest {
        if !token.starts_with('-') {
            return None;
        }
        let name = token.split('=').next().unwrap_or(token);
        let selective = match runner {
            Runner::Cargo => matches!(
                name,
                "--test" | "--tests" | "--lib" | "--bins" | "--bin" | "--doc" | "--doctests"
                    | "--examples" | "--example" | "--benches" | "--bench" | "--all-targets"
                    | "-p" | "--package" | "--exclude"
            ),
            Runner::Pytest => matches!(
                name,
                "-k" | "-m" | "--ignore" | "--deselect" | "--co" | "--collect-only"
            ),
            Runner::Go => matches!(name, "-run" | "-skip"),
        };
        if selective {
            return None;
        }
        flags.push(token.to_string());
    }
    Some((runner, flags))
}

/// Files the active plan's sessions wrote (journal `file_diff` chain),
/// project-relative with forward slashes. Plan attribution or legacy
/// unattributed records; anything else belongs to another plan.
fn changed_files(root: &Path, plan_id: &str) -> Vec<String> {
    let mut out = Vec::new();
    let dir = root.join(".sqwai").join("journal");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        if entry.path().extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        for line in text.lines() {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if value.get("kind").and_then(|k| k.as_str()) != Some("file_diff") {
                continue;
            }
            let mine = value
                .get("plan")
                .and_then(|p| p.as_str())
                .is_none_or(|p| p == plan_id);
            if !mine {
                continue;
            }
            if let Some(path) = value
                .get("path")
                .and_then(|p| p.as_str())
                .map(|p| p.replace('\\', "/"))
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
            {
                if !out.contains(&path) {
                    out.push(path);
                }
            }
        }
    }
    out.sort();
    out
}

/// True when the file is a test file: indexed nodes with a `test` role
/// win; path conventions only speak for files the index never saw.
fn is_test_file(
    store: Option<&crate::agent::graph::SqliteGraphStore>,
    path: &str,
) -> bool {
    if let Some(store) = store
        && let Ok(nodes) = store.nodes_in_file(path)
        && !nodes.is_empty()
    {
        return nodes
            .iter()
            .any(|n| n.roles.iter().any(|r| r == "test"));
    }
    test_convention(path)
}

fn test_convention(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let base = lower.rsplit('/').next().unwrap_or(&lower);
    lower.contains("/tests/")
        || lower.contains("/test/")
        || lower.contains("/__tests__/")
        || base.starts_with("test_")
        || base.starts_with("test-")
        || base.contains("_test.")
        || base.contains(".spec.")
}

/// Test-role symbol names declared in a file, for `-run` narrowing.
fn test_names(
    store: Option<&crate::agent::graph::SqliteGraphStore>,
    path: &str,
) -> Vec<String> {
    let Some(store) = store else {
        return Vec::new();
    };
    let Ok(nodes) = store.nodes_in_file(path) else {
        return Vec::new();
    };
    nodes
        .into_iter()
        .filter(|n| n.roles.iter().any(|r| r == "test"))
        .filter_map(|n| n.name)
        .filter(|name| {
            !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_')
        })
        .collect()
}

/// Quote for shell argv when the path needs it (double quotes survive
/// sh, cmd and powershell alike).
fn quote(path: &str) -> String {
    if path.chars().any(|c| c.is_whitespace()) {
        format!("\"{path}\"")
    } else {
        path.to_string()
    }
}

/// Covering test files for the changed paths: the changed test files
/// themselves, same-directory test files (same-package tests never
/// import their target), and the reverse-import closure. `None` when
/// there is nothing to select from — the caller runs the full command.
fn covering_tests(
    store: Option<&crate::agent::graph::SqliteGraphStore>,
    changed: &[String],
    root: &Path,
) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    let mut push = |path: &str| {
        if !files.contains(&path.to_string()) {
            files.push(path.to_string());
        }
    };
    // reverse-import closure over the changed files
    if let Some(store) = store {
        let mut seen = std::collections::HashSet::new();
        let mut boundary: Vec<String> = changed
            .iter()
            .map(|p| format!("file:{p}"))
            .collect();
        let mut depth = 0;
        let mut over_cap = false;
        while !boundary.is_empty() && depth <= CLOSURE_DEPTH {
            let mut next = Vec::new();
            for key in std::mem::take(&mut boundary) {
                if !seen.insert(key.clone()) {
                    continue;
                }
                if seen.len() > CLOSURE_NODE_CAP {
                    over_cap = true;
                    break;
                }
                let Ok(proj) = store.neighbors(
                    &key,
                    NeighborQuery {
                        direction: Direction::Incoming,
                        depth: 1,
                        limit: 100,
                    },
                ) else {
                    continue;
                };
                for node in &proj.nodes {
                    if !seen.contains(&node.stable_key) {
                        next.push(node.stable_key.clone());
                    }
                    // the closure walks importers, not tests: keep only
                    // files that actually hold tests (a changed test file
                    // re-enters below through the changed loop)
                    if node.kind == crate::agent::graph::NodeKind::File
                        && let Some(path) = node.path.clone()
                        && is_test_file(Some(store), &path)
                    {
                        push(&path);
                    }
                }
            }
            if over_cap {
                break;
            }
            boundary = next;
            depth += 1;
        }
        if over_cap {
            return Vec::new();
        }
    }
    for path in changed {
        // a changed test file runs itself
        if is_test_file(store, path) {
            push(path);
        }
        // same-directory test files: same-package tests never import
        // their target, so the import closure cannot see them
        let parent = std::path::Path::new(path)
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let dir = if parent.is_empty() || parent == "." {
            root.to_path_buf()
        } else {
            root.join(&parent)
        };
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if !p.is_file() {
                    continue;
                }
                let rel = p
                    .strip_prefix(root)
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                if rel.is_empty() {
                    continue;
                }
                if is_test_file(store, &rel) {
                    push(&rel);
                }
            }
        }
    }
    files.sort();
    files
}

/// Select the subset invocation for a verify run, or `None` for the
/// authored command. Pure decision apart from journal/graph reads; never
/// fails the turn — every doubt returns `None`.
pub fn select_command(root: &Path, plan_id: &str, command: &str) -> Option<Impacted> {
    let (runner, flags) = split_runner(command)?;
    let changed = changed_files(root, plan_id);
    if changed.is_empty() {
        return None;
    }
    let store = crate::agent::graph::SqliteGraphStore::open(root).ok();
    let store_ref = store.as_ref();
    let files = covering_tests(store_ref, &changed, root);
    if files.is_empty() || files.len() > MAX_IMPACT_FILES {
        return None;
    }
    let flag_str = if flags.is_empty() {
        String::new()
    } else {
        format!(" {}", flags.join(" "))
    };
    let command = match runner {
        Runner::Pytest => {
            let listed: Vec<String> = files.iter().map(|f| quote(f)).collect();
            format!("pytest{flag_str} {}", listed.join(" "))
        }
        Runner::Go => {
            let mut dirs: Vec<String> = files
                .iter()
                .map(|f| match std::path::Path::new(f).parent().map(|p| p.to_string_lossy().into_owned()) {
                    Some(d) if !d.is_empty() && d != "." => format!("./{d}"),
                    _ => ".".to_string(),
                })
                .collect();
            dirs.sort();
            dirs.dedup();
            let mut names: Vec<String> = files
                .iter()
                .flat_map(|f| test_names(store_ref, f))
                .collect();
            names.sort();
            names.dedup();
            let run = if !names.is_empty() && names.len() <= MAX_IMPACT_NAMES {
                format!(" -run \"^({})$\"", names.join("|"))
            } else {
                String::new()
            };
            format!("go test{flag_str}{run} {}", dirs.join(" "))
        }
        Runner::Cargo => {
            // integration targets only: unit tests inside src/ have no
            // file address on the cargo CLI, so any src/ impact runs full
            let mut stems = Vec::new();
            for f in &files {
                let Some(stem) = f
                    .strip_prefix("tests/")
                    .and_then(|rest| rest.strip_suffix(".rs"))
                    .filter(|rest| !rest.contains('/'))
                else {
                    return None;
                };
                stems.push(format!("--test {stem}"));
            }
            if stems.is_empty() {
                return None;
            }
            format!("cargo test{flag_str} {}", stems.join(" "))
        }
    };
    let shown = if files.len() <= 5 {
        files.join(", ")
    } else {
        format!("{}, … (+{} more)", files[..5].join(", "), files.len() - 5)
    };
    Some(Impacted {
        command,
        files,
        note: format!(
            "impact: covering tests first ({shown}); full suite still required at complete"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_runner_accepts_bare_and_benign_flags() {
        assert_eq!(
            split_runner("cargo test"),
            Some((Runner::Cargo, vec![]))
        );
        assert_eq!(
            split_runner("cargo test --release"),
            Some((Runner::Cargo, vec!["--release".to_string()]))
        );
        assert_eq!(
            split_runner("pytest -q --tb=short"),
            Some((Runner::Pytest, vec!["-q".to_string(), "--tb=short".to_string()]))
        );
        assert_eq!(split_runner("go test"), Some((Runner::Go, vec![])));
        // leading env assignments are not selection
        assert_eq!(
            split_runner("RUST_BACKTRACE=1 cargo test"),
            Some((Runner::Cargo, vec![]))
        );
    }

    #[test]
    fn split_runner_refuses_selection_and_strangers() {
        // user-narrowed invocations run as authored
        assert_eq!(split_runner("pytest tests/a.py"), None);
        assert_eq!(split_runner("pytest -k foo"), None);
        assert_eq!(split_runner("pytest -k=foo"), None);
        assert_eq!(split_runner("cargo test foo"), None);
        assert_eq!(split_runner("cargo test --test foo"), None);
        assert_eq!(split_runner("go test ./..."), None);
        assert_eq!(split_runner("go test -run TestX"), None);
        // unknown runners are not impactable
        assert_eq!(split_runner("make test"), None);
        assert_eq!(split_runner("npm test"), None);
        assert_eq!(split_runner("echo hi"), None);
        assert_eq!(split_runner(""), None);
    }

    fn file_node(path: &str, test_role: bool) -> crate::agent::graph::Node {
        crate::agent::graph::Node {
            stable_key: format!("file:{path}"),
            kind: crate::agent::graph::NodeKind::File,
            name: None,
            path: Some(path.to_string()),
            language: None,
            line_start: None,
            line_end: None,
            signature: None,
            roles: if test_role {
                vec!["test".to_string()]
            } else {
                Vec::new()
            },
            properties: Default::default(),
            content_hash: None,
        }
    }

    fn edge(from: &str, to: &str) -> crate::agent::graph::Edge {
        crate::agent::graph::Edge {
            from: from.to_string(),
            to: to.to_string(),
            kind: "imports".to_string(),
            confidence: None,
            source: None,
            source_hash: None,
            limitations: Vec::new(),
            properties: Default::default(),
        }
    }

    fn impact_root() -> tempfile::TempDir {
        use crate::agent::graph::GraphStore;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "a\n").unwrap();
        std::fs::write(dir.path().join("tests/test_a.py"), "t\n").unwrap();
        let mut store =
            crate::agent::graph::SqliteGraphStore::open(dir.path()).unwrap();
        store
            .apply_batch(
                &[
                    file_node("src/a.rs", false),
                    file_node("tests/test_a.py", true),
                ],
                &[edge("file:tests/test_a.py", "file:src/a.rs")],
            )
            .unwrap();
        // the agent wrote src/a.rs this session under the plan
        let mut journal =
            crate::agent::journal::Journal::open(dir.path(), "sess").unwrap();
        journal.set_attribution(Some("1".into()), Some("plan-1".into()), "main");
        journal
            .append("file_diff", serde_json::json!({"path": "src/a.rs"}))
            .unwrap();
        dir
    }

    #[test]
    fn pytest_selects_covering_tests() {
        let dir = impact_root();
        let impacted = select_command(dir.path(), "plan-1", "pytest -q")
            .expect("pytest selects");
        assert_eq!(impacted.files, vec!["tests/test_a.py".to_string()]);
        assert_eq!(impacted.command, "pytest -q tests/test_a.py");
        assert!(impacted.note.contains("full suite still required at complete"));
        // unknown runner / no diffs / other plan: full command (None)
        assert_eq!(select_command(dir.path(), "plan-1", "make test"), None);
        assert_eq!(select_command(dir.path(), "plan-9", "pytest"), None);
        std::fs::remove_dir_all(dir.path()).ok();
    }

    #[test]
    fn cargo_runs_full_for_unmappable_targets() {
        let dir = impact_root();
        // tests/test_a.py is a test file, but cargo cannot address a .py
        // target — and src/ unit tests have no file address at all — so
        // the whole selection is unmappable and the full command stands
        assert_eq!(select_command(dir.path(), "plan-1", "cargo test"), None);
        std::fs::remove_dir_all(dir.path()).ok();
    }

    #[test]
    fn cargo_selects_integration_targets() {
        use crate::agent::graph::GraphStore;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("tests/i_a.rs"), "t\n").unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "a\n").unwrap();
        let mut store =
            crate::agent::graph::SqliteGraphStore::open(dir.path()).unwrap();
        store
            .apply_batch(
                &[file_node("tests/i_a.rs", true), file_node("src/a.rs", false)],
                &[edge("file:tests/i_a.rs", "file:src/a.rs")],
            )
            .unwrap();
        let mut journal =
            crate::agent::journal::Journal::open(dir.path(), "sess").unwrap();
        journal.set_attribution(Some("1".into()), Some("plan-1".into()), "main");
        journal
            .append("file_diff", serde_json::json!({"path": "tests/i_a.rs"}))
            .unwrap();
        let impacted =
            select_command(dir.path(), "plan-1", "cargo test").expect("cargo selects");
        assert_eq!(impacted.files, vec!["tests/i_a.rs".to_string()]);
        assert_eq!(impacted.command, "cargo test --test i_a");
        std::fs::remove_dir_all(dir.path()).ok();
    }

    #[test]
    fn go_selects_package_dirs() {
        use crate::agent::graph::GraphStore;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("pkg")).unwrap();
        std::fs::write(dir.path().join("pkg/f.go"), "p\n").unwrap();
        std::fs::write(dir.path().join("pkg/f_test.go"), "t\n").unwrap();
        let mut store =
            crate::agent::graph::SqliteGraphStore::open(dir.path()).unwrap();
        store
            .apply_batch(
                &[file_node("pkg/f.go", false), file_node("pkg/f_test.go", true)],
                &[edge("file:pkg/f_test.go", "file:pkg/f.go")],
            )
            .unwrap();
        let mut journal =
            crate::agent::journal::Journal::open(dir.path(), "sess").unwrap();
        journal.set_attribution(Some("1".into()), Some("plan-1".into()), "main");
        journal
            .append("file_diff", serde_json::json!({"path": "pkg/f.go"}))
            .unwrap();
        let impacted =
            select_command(dir.path(), "plan-1", "go test").expect("go selects");
        assert_eq!(impacted.files, vec!["pkg/f_test.go".to_string()]);
        assert_eq!(impacted.command, "go test ./pkg");
        std::fs::remove_dir_all(dir.path()).ok();
    }
}
