//! Built-in tool registry (phase 2).
//!
//! Each tool declares its JSON schema for the model and a handler. Handlers
//! receive a [`ToolCtx`] carrying the project root and session-scoped guard
//! state (which files were read, checkpoint journal).

mod ctx;
mod dispatch;
mod exec;
mod fs;
mod git;
mod outline;
mod policy;
mod specs;
mod verify;
pub(crate) mod web;

pub(crate) use ctx::{ReadState, ToolCtx};
pub(crate) use dispatch::{FileDiff, Outcome, bg_running_commands, execute, kill_remaining_jobs};
pub(crate) use git::porcelain_paths;
pub(crate) use policy::{
    bash_scope_hit, register_mention_prereads, register_subagent_scope, take_mention_prereads,
    take_subagent_scope,
};
pub(crate) use specs::{
    Kind, call_args_for_journal, call_path, call_summary, decode_child_output, is_mutating_call,
    merge_specs, tool_names, tool_specs, trim_middle,
};

#[cfg(test)]
mod tests {
    use super::dispatch::plan_op;
    use super::*;
    use crate::agent::tools::ctx::MIN_PLAN_BUDGET_TOKENS;
    use crate::plan;
    use serde_json::json;
    use std::fs;
    use std::path::{Path, PathBuf};

    #[test]
    fn call_path_extracts_file_targets() {
        let args = |pairs: &[(&str, &str)]| {
            let mut m = serde_json::Map::new();
            for (k, v) in pairs {
                m.insert(k.to_string(), serde_json::Value::String(v.to_string()));
            }
            serde_json::Value::Object(m)
        };
        assert_eq!(
            call_path("read", &args(&[("file_path", "src/a.rs")])),
            Some("src/a.rs".to_string())
        );
        assert_eq!(
            call_path("ls", &args(&[("path", "src")])),
            Some("src".to_string())
        );
        assert_eq!(call_path("bash", &args(&[("command", "ls")])), None);
        assert_eq!(call_path("read", &args(&[])), None);
        assert_eq!(call_path("read", &args(&[("file_path", "")])), None);
    }

    fn proj() -> (ToolCtx, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "sqwai-tools-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/main.rs"), "fn main() {}\n// TODO\n").unwrap();
        fs::write(dir.join("README.md"), "# demo\n").unwrap();
        let ctx = ToolCtx::new(&dir);
        // git init so checkpoints work
        std::process::Command::new("git")
            .current_dir(&dir)
            .args(["init", "-q"])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .current_dir(&dir)
            .args(["config", "user.email", "t@t"])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .current_dir(&dir)
            .args(["config", "user.name", "t"])
            .status()
            .unwrap();
        (ctx, dir)
    }

    #[test]
    fn read_only_context_rejects_mutations_but_allows_reads() {
        let (_, dir) = proj();
        let mut ctx = ToolCtx::with_read_only(&dir, true);
        let denied = execute(
            &mut ctx,
            "write",
            &json!({"file_path": "src/new.rs", "content": "fn main() {}\n"}),
        );
        assert!(!denied.ok);
        assert!(denied.output.contains("read-only"));
        let allowed = execute(&mut ctx, "read", &json!({"file_path": "README.md"}));
        assert!(allowed.ok);
    }

    /// A writer subagent stays inside its declared scope: same file and
    /// nested paths pass, siblings refuse with a structured code. Reads
    /// are unaffected, and the main agent (no scope) is unaffected too.
    #[test]
    fn subagent_write_scope_confines_file_mutations() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "scoped child",
                "acceptance": ["manual: eyeball it"],
                "steps": [{"title": "work"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        ctx.subagent_step = Some(plan::StepContext {
            plan_id: plan_id.clone(),
            step_id: "1".into(),
            step_epoch: 0,
        });
        ctx.subagent_write_paths = Some(vec!["src".to_string()]);

        let inside = execute(
            &mut ctx,
            "write",
            &json!({"file_path": "src/child.rs", "content": "fresh\n"}),
        );
        assert!(inside.ok, "{}", inside.output);

        let outside = execute(
            &mut ctx,
            "write",
            &json!({"file_path": "notes.txt", "content": "elsewhere\n"}),
        );
        assert!(!outside.ok, "{}", outside.output);
        assert!(
            outside.output.contains("subagent_scope"),
            "{}",
            outside.output
        );

        let read = execute(&mut ctx, "read", &json!({"file_path": "README.md"}));
        assert!(read.ok, "{}", read.output);
        assert!(!dir.join("notes.txt").exists());
        fs::remove_dir_all(&dir).ok();
    }

    /// The scope gate is a boundary, not a suggestion: bash redirects,
    /// `git_stage` paths and unbounded `all:true` commits obey it too.
    /// Pre-fix only write/edit/multi_edit/patch were confined — a scoped
    /// child wrote anywhere through `echo x > ../outside`.
    #[test]
    fn subagent_write_scope_confines_bash_git_and_commit_all() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "scoped child",
                "acceptance": ["manual: eyeball it"],
                "steps": [{"title": "work"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        ctx.subagent_step = Some(plan::StepContext {
            plan_id: plan_id.clone(),
            step_id: "1".into(),
            step_epoch: 0,
        });
        ctx.subagent_write_paths = Some(vec!["src".to_string()]);

        // a bare command with no write targets runs
        let plain = execute(&mut ctx, "bash", &json!({"command": "echo hi"}));
        assert!(plain.ok, "{}", plain.output);

        // a redirect outside the scope refuses BEFORE executing
        let outside = execute(&mut ctx, "bash", &json!({"command": "echo x > notes.txt"}));
        assert!(!outside.ok, "{}", outside.output);
        assert!(
            outside.output.contains("subagent_scope"),
            "{}",
            outside.output
        );
        assert!(
            !dir.join("notes.txt").exists(),
            "refused write must not land"
        );

        // the same redirect inside the scope runs
        let inside = execute(
            &mut ctx,
            "bash",
            &json!({"command": "echo x > src/inside.txt"}),
        );
        assert!(inside.ok, "{}", inside.output);
        assert!(dir.join("src/inside.txt").exists());

        // quoted operators are not redirects
        let narrated = execute(&mut ctx, "bash", &json!({"command": "echo \"a > b\""}));
        assert!(narrated.ok, "{}", narrated.output);

        // git_stage paths obey the scope; all:true is unbounded, refused.
        // (`..` escapes die in the tool's own jail — also refused, other code.)
        let stage_out = execute(&mut ctx, "git_stage", &json!({"paths": ["notes.txt"]}));
        assert!(!stage_out.ok, "{}", stage_out.output);
        assert!(
            stage_out.output.contains("subagent_scope"),
            "{}",
            stage_out.output
        );
        let stage_escape = execute(&mut ctx, "git_stage", &json!({"paths": ["../outside.txt"]}));
        assert!(!stage_escape.ok, "{}", stage_escape.output);
        let stage_all = execute(&mut ctx, "git_stage", &json!({"all": true}));
        assert!(!stage_all.ok, "{}", stage_all.output);
        assert!(
            stage_all.output.contains("subagent_scope"),
            "{}",
            stage_all.output
        );

        // git_commit all:true sweeps the whole tree, refused; a plain
        // commit only seals the (gated) stage, so it passes the gate
        let commit_all = execute(
            &mut ctx,
            "git_commit",
            &json!({"message": "sweep", "all": true}),
        );
        assert!(!commit_all.ok, "{}", commit_all.output);
        assert!(
            commit_all.output.contains("subagent_scope"),
            "{}",
            commit_all.output
        );
        let commit_plain = execute(&mut ctx, "git_commit", &json!({"message": "seal"}));
        assert!(
            !commit_plain.output.contains("subagent_scope"),
            "{}",
            commit_plain.output
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// Audit H2: `nohup`/`nice`/`timeout` are transparent execution
    /// prefixes. The scope gate's skip list covered only sudo/doas/env, so
    /// the wrapper became "the binary", no write target was extracted, and
    /// an out-of-scope `mv`/`cp` sailed through.
    #[test]
    fn subagent_scope_sees_through_wrapper_prefixes() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "scoped child",
                "acceptance": ["manual: eyeball it"],
                "steps": [{"title": "work"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        ctx.subagent_step = Some(plan::StepContext {
            plan_id,
            step_id: "1".into(),
            step_epoch: 0,
        });
        ctx.subagent_write_paths = Some(vec!["src".to_string()]);
        fs::write(dir.join("src/payload.txt"), "x\n").unwrap();

        for cmd in [
            "nohup mv src/payload.txt notes-escape.txt",
            "nice -n 10 cp src/payload.txt notes-escape.txt",
            "timeout 30 mv src/payload.txt notes-escape.txt",
        ] {
            let out = execute(&mut ctx, "bash", &json!({"command": cmd}));
            assert!(!out.ok, "wrapper prefix escaped the scope: {cmd}");
            assert!(
                out.output.contains("subagent_scope"),
                "expected subagent_scope refusal for `{cmd}`: {}",
                out.output
            );
        }
        assert!(
            !dir.join("notes-escape.txt").exists(),
            "refused write must not land"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// §11 operand roles: a scoped child may read from anywhere and write
    /// only inside its scope. A mover's sources are reads, `/dev/null` is a
    /// sink, a sed script is not a file — none of them may refuse.
    #[test]
    fn subagent_scope_reads_outside_and_writes_inside() {
        let (ctx, dir) = proj();
        let scope = ["src".to_string()];
        for cmd in [
            "mv ../old.rs ./src/new.rs",
            "cp /etc/hosts src/hosts.copy",
            "rsync -a ../sibling/ src/sibling/",
            "make test 2>/dev/null",
            "sed -i s/old/new/ src/main.rs",
            "echo done > 'src/status.txt'",
        ] {
            assert_eq!(
                bash_scope_hit(&ctx, &scope, cmd),
                None,
                "legal in-scope write must not refuse: {cmd}"
            );
        }
        for cmd in [
            "mv src/x.rs ../x.rs",
            "cp src/x.rs notes.md",
            "echo hi > src/../notes.md",
            "nohup tee notes.md < src/main.rs",
        ] {
            assert!(
                bash_scope_hit(&ctx, &scope, cmd).is_some(),
                "out-of-scope write must refuse: {cmd}"
            );
        }
        fs::remove_dir_all(&dir).ok();
    }

    /// Audit H3: scope was compared lexically while the OS resolves
    /// symlinks — a link inside scope `a/` pointing at sibling `b/` let a
    /// writer confined to `a/` mutate `b/`. Scope decisions must run on
    /// canonical paths.
    #[test]
    fn subagent_scope_follows_symlinks_canonically() {
        let (mut ctx, dir) = proj();
        fs::create_dir_all(dir.join("b")).unwrap();
        // link a/link -> ../b ; skip the test where symlinks need privileges
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(dir.join("b"), dir.join("src/link")).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(dir.join("b"), dir.join("src/link")).is_ok();
        if !made {
            eprintln!("symlink creation unsupported here; skipping");
            return;
        }
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "scoped child",
                "acceptance": ["manual: eyeball it"],
                "steps": [{"title": "work"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        ctx.subagent_step = Some(plan::StepContext {
            plan_id,
            step_id: "1".into(),
            step_epoch: 0,
        });
        ctx.subagent_write_paths = Some(vec!["src".to_string()]);

        // a file-tool write through the link lands in b/ — outside scope
        let out = execute(
            &mut ctx,
            "write",
            &json!({"file_path": "src/link/escape.txt", "content": "x\n"}),
        );
        assert!(!out.ok, "symlink write escaped the scope: {}", out.output);
        assert!(out.output.contains("subagent_scope"), "{}", out.output);
        assert!(!dir.join("b/escape.txt").exists(), "write must not land");

        // the same through a bash redirect
        let out = execute(
            &mut ctx,
            "bash",
            &json!({"command": "echo x > src/link/escape2.txt"}),
        );
        assert!(
            !out.ok,
            "symlink redirect escaped the scope: {}",
            out.output
        );
        assert!(out.output.contains("subagent_scope"), "{}", out.output);
        assert!(!dir.join("b/escape2.txt").exists(), "write must not land");

        // a plain in-scope write still works
        let out = execute(
            &mut ctx,
            "write",
            &json!({"file_path": "src/ok.txt", "content": "x\n"}),
        );
        assert!(out.ok, "{}", out.output);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_then_edit_flow_and_guards() {
        let (mut ctx, dir) = proj();

        // overwriting an existing file without a read stays refused:
        // blind destruction is what exact matching cannot catch
        let o = execute(
            &mut ctx,
            "write",
            &json!({"file_path": "src/main.rs", "content": "wiped\n"}),
        );
        assert!(!o.ok, "overwrite must require a prior read");
        assert!(o.output.contains("was not read"), "{}", o.output);

        // mass replacement without a read stays refused for the same reason
        let o = execute(
            &mut ctx,
            "edit",
            &json!({"file_path": "src/main.rs", "old_string": "fn", "new_string": "FN", "replace_all": true}),
        );
        assert!(!o.ok, "replace_all must require a prior read");

        // a single exact edit is self-validating: no prior read needed
        let o = execute(
            &mut ctx,
            "edit",
            &json!({"file_path": "src/main.rs", "old_string": "TODO", "new_string": "DONE"}),
        );
        assert!(o.ok, "exact edit needs no prior read: {}", o.output);
        assert_eq!(
            fs::read_to_string(dir.join("src/main.rs")).unwrap(),
            "fn main() {}\n// DONE\n"
        );

        // ...but a blind guess fails safe instead of writing
        let o = execute(
            &mut ctx,
            "edit",
            &json!({"file_path": "src/main.rs", "old_string": "NOPE", "new_string": "DONE"}),
        );
        assert!(!o.ok, "mismatch must fail safe");
        assert_eq!(
            fs::read_to_string(dir.join("src/main.rs")).unwrap(),
            "fn main() {}\n// DONE\n"
        );

        // read marks the file; working from a stale read still refuses
        // even exact edits — the file moved underneath
        let o = execute(&mut ctx, "read", &json!({"file_path": "src/main.rs"}));
        assert!(o.ok, "{}", o.output);
        fs::write(dir.join("src/main.rs"), "fn main() {}\n// EXTERNAL\n").unwrap();
        let o = execute(
            &mut ctx,
            "edit",
            &json!({"file_path": "src/main.rs", "old_string": "EXTERNAL", "new_string": "DONE"}),
        );
        assert!(!o.ok, "stale read must refuse even exact edits");
        assert!(
            o.output.contains("changed since you read it"),
            "{}",
            o.output
        );
        // checkpoint journal got entries from the mutation above
        assert!(!ctx.journal.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn multi_edit_without_read_is_atomic_and_allowed() {
        let (mut ctx, dir) = proj();
        // all legs valid: applies without any prior read (validated
        // against the evolving text before touching disk)
        let o = execute(
            &mut ctx,
            "multi_edit",
            &json!({"file_path": "src/main.rs", "edits": [
                {"old_string": "TODO", "new_string": "DONE"},
                {"old_string": "fn main() {}", "new_string": "fn main() { /* x */ }"},
            ]}),
        );
        assert!(o.ok, "atomic multi_edit needs no prior read: {}", o.output);
        // one bad leg aborts the whole batch: nothing lands on disk
        let o = execute(
            &mut ctx,
            "multi_edit",
            &json!({"file_path": "src/main.rs", "edits": [
                {"old_string": "DONE", "new_string": "DONE2"},
                {"old_string": "MISSING", "new_string": "X"},
            ]}),
        );
        assert!(!o.ok, "bad leg must abort the batch");
        assert_eq!(
            fs::read_to_string(dir.join("src/main.rs")).unwrap(),
            "fn main() { /* x */ }\n// DONE\n"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_output_reports_totals_and_spills_when_truncated() {
        let (mut ctx, dir) = proj();
        // small file: totals footer, no spill
        let o = execute(&mut ctx, "read", &json!({"file_path": "src/main.rs"}));
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("of 2 total"), "{}", o.output);
        assert!(!o.output.contains("sqwai-spill"), "{}", o.output);

        // big file: truncated with totals, full text spilled to a file the
        // tools themselves can grep and page
        let big: String = (0..2500).map(|i| format!("line {i}\n")).collect();
        fs::write(dir.join("big.rs"), &big).unwrap();
        let o = execute(&mut ctx, "read", &json!({"file_path": "big.rs"}));
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("of 2500 total"), "{}", o.output);
        assert!(o.output.contains("sqwai-spill/read-"), "{}", o.output);
        assert!(
            o.output.contains("do not re-read the whole file"),
            "{}",
            o.output
        );
        let spill = o
            .output
            .lines()
            .find_map(|line| {
                let start = line.find("sqwai-spill/read-")?;
                Some(
                    line[start..]
                        .split_whitespace()
                        .next()
                        .unwrap_or_default()
                        .trim_end_matches([':', ',', '.'])
                        .to_string(),
                )
            })
            .expect("spill path in output");
        let back = execute(&mut ctx, "read", &json!({"file_path": spill, "limit": 5}));
        assert!(back.ok, "spill must be readable: {}", back.output);
        assert!(back.output.contains("line 0"), "{}", back.output);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn path_escape_is_rejected() {
        let (mut ctx, _dir) = proj();
        for p in ["../outside.txt", "..\\outside.txt", "C:\\Windows\\win.ini"] {
            let o = execute(&mut ctx, "read", &json!({"file_path": p}));
            assert!(!o.ok, "{p} must be rejected");
        }
    }

    /// A dirty tree is not the same as a dirty tree *by this session*. Without
    /// the distinction a model that finds unfamiliar edits assumes authorship
    /// and starts committing work it never took on — measured in a live
    /// session, where another agent's uncommitted edits triggered exactly that.
    #[test]
    fn git_status_marks_paths_this_session_never_wrote() {
        let (mut ctx, dir) = proj();
        // foreign dirt: it appears in the tree with no tool call behind it
        fs::write(dir.join("foreign.rs"), "fn f() {}\n").unwrap();

        let out = execute(&mut ctx, "git_status", &json!({}));
        assert!(out.ok, "{}", out.output);
        assert!(
            out.output.contains("authorship") && out.output.contains("foreign.rs"),
            "the foreign path must be named: {}",
            out.output
        );

        // what this session wrote through a tool is not foreign — the journal
        // says so, and that is the whole basis of the line
        let mine = execute(
            &mut ctx,
            "write",
            &json!({"file_path": "mine.rs", "content": "fn m() {}\n"}),
        );
        assert!(mine.ok, "{}", mine.output);
        crate::agent::journal::Journal::open(&dir, &ctx.session_id)
            .unwrap()
            .append("file_diff", json!({"path": "mine.rs", "added": 1}))
            .unwrap();

        let again = execute(&mut ctx, "git_status", &json!({}));
        let tail = again
            .output
            .split("authorship:")
            .nth(1)
            .unwrap_or_default()
            .to_string();
        assert!(tail.contains("foreign.rs"), "again: {}", again.output);
        assert!(
            !tail.contains("mine.rs"),
            "the session's own write must not be called foreign: {tail}"
        );

        // a clean tree gets no line at all
        let clean = std::env::temp_dir().join(format!("sqwai-author-clean-{}", std::process::id()));
        std::fs::create_dir_all(&clean).unwrap();
        std::process::Command::new("git")
            .current_dir(&clean)
            .args(["init", "-q"])
            .status()
            .unwrap();
        let mut clean_ctx = ToolCtx::new(&clean);
        clean_ctx.session_id = "clean-sess".to_string();
        let quiet = execute(&mut clean_ctx, "git_status", &json!({}));
        assert!(quiet.ok, "{}", quiet.output);
        assert!(
            !quiet.output.contains("authorship"),
            "nothing dirty, nothing to say: {}",
            quiet.output
        );
        fs::remove_dir_all(&clean).ok();
        fs::remove_dir_all(&dir).ok();
    }

    /// §2.0: file tools must refuse host-owned state under `.sqwai/`. Without
    /// this the model can rewrite the goal and mark steps done with `write`,
    /// which would contradict §8.1 ("a plan's goal cannot be changed by any
    /// model action").
    #[test]
    fn host_owned_state_is_unreachable_from_file_tools() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({"op":"create","goal":"original goal","constraints":["keep the format"],
                    "criteria":["note"],"steps":[{"title":"first"}]}),
        );
        assert!(created.ok, "{}", created.output);
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        fs::create_dir_all(dir.join(".sqwai/journal")).unwrap();
        fs::write(dir.join(".sqwai/journal/live.jsonl"), "").unwrap();

        for path in [
            format!(".sqwai/plans/{plan_id}.json"),
            ".sqwai/journal/live.jsonl".to_string(),
            ".sqwai/journal/forged.jsonl".to_string(),
            ".sqwai/memory/MEMORY.md".to_string(),
            ".sqwai/checkpoints/head.json".to_string(),
            ".sqwai/exports/session.md".to_string(),
            ".sqwai".to_string(),
        ] {
            for tool in ["read", "ls", "write", "edit"] {
                let out = execute(
                    &mut ctx,
                    tool,
                    &json!({"file_path": path, "path": path, "content": "x",
                            "old_string": "a", "new_string": "b"}),
                );
                assert!(!out.ok, "{tool} must refuse {path}: {}", out.output);
                assert!(
                    out.output.contains("host-owned state"),
                    "{tool} on {path} must say why: {}",
                    out.output
                );
                // the hint is model-facing: it must name tools that exist.
                // It used to point at a graph store that was cut in B2.
                assert!(
                    !out.output.contains("graph"),
                    "{tool} on {path} names a retired mechanism: {}",
                    out.output
                );
                assert!(
                    out.output.contains("memory_write"),
                    "{tool} on {path} must offer the way in: {}",
                    out.output
                );
            }
        }

        // the goal survived every attempt above
        let after = plan::open_active(&dir).unwrap().unwrap();
        assert_eq!(after.goal.text, "original goal");
        assert_eq!(after.constraints, vec!["keep the format".to_string()]);

        let _ = fs::remove_dir_all(dir);
    }

    /// The two documented exceptions stay reachable: project skills are
    /// committed and edited like any other file, and so is the project config.
    #[test]
    fn skills_and_project_config_stay_reachable() {
        let (mut ctx, dir) = proj();
        fs::create_dir_all(dir.join(".sqwai/skills/demo")).unwrap();

        let written = execute(
            &mut ctx,
            "write",
            &json!({"file_path": ".sqwai/skills/demo/SKILL.md", "content": "# demo\n"}),
        );
        assert!(written.ok, "{}", written.output);
        let read = execute(
            &mut ctx,
            "read",
            &json!({"file_path": ".sqwai/skills/demo/SKILL.md"}),
        );
        assert!(read.ok, "{}", read.output);

        let config = execute(
            &mut ctx,
            "write",
            &json!({"file_path": ".sqwai/config.toml", "content": "scope_guard = \"warn\"\n"}),
        );
        assert!(config.ok, "{}", config.output);

        let _ = fs::remove_dir_all(dir);
    }

    /// A symlink inside the project pointing at host-owned state must not be a
    /// way around the jail: the check runs on the canonicalized path.
    #[cfg(unix)]
    #[test]
    fn symlink_into_host_state_is_refused() {
        let (mut ctx, dir) = proj();
        fs::create_dir_all(dir.join(".sqwai/plans")).unwrap();
        fs::write(dir.join(".sqwai/plans/p.json"), "{}").unwrap();
        std::os::unix::fs::symlink(dir.join(".sqwai/plans"), dir.join("shortcut")).unwrap();

        let out = execute(&mut ctx, "read", &json!({"file_path": "shortcut/p.json"}));
        assert!(!out.ok, "symlink must not bypass the jail: {}", out.output);
        assert!(out.output.contains("host-owned state"), "{}", out.output);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn overwrite_requires_read_new_file_does_not() {
        let (mut ctx, dir) = proj();
        // brand-new file: fine
        let o = execute(
            &mut ctx,
            "write",
            &json!({"file_path": "docs/new.md", "content": "hello"}),
        );
        assert!(o.ok, "{}", o.output);
        assert!(dir.join("docs/new.md").exists());

        // existing-but-unread: denied
        let o = execute(
            &mut ctx,
            "write",
            &json!({"file_path": "README.md", "content": "clobber"}),
        );
        assert!(!o.ok, "blind overwrite must be denied");
        assert_eq!(
            fs::read_to_string(dir.join("README.md")).unwrap(),
            "# demo\n"
        );

        // after read: allowed
        execute(&mut ctx, "read", &json!({"file_path": "README.md"}));
        let o = execute(
            &mut ctx,
            "write",
            &json!({"file_path": "README.md", "content": "rewritten"}),
        );
        assert!(o.ok);
        assert_eq!(
            fs::read_to_string(dir.join("README.md")).unwrap(),
            "rewritten"
        );
    }

    #[test]
    fn edit_non_unique_fails_atomically() {
        let (mut ctx, dir) = proj();
        fs::write(dir.join("dup.txt"), "x x x\n").unwrap();
        execute(&mut ctx, "read", &json!({"file_path": "dup.txt"}));
        let o = execute(
            &mut ctx,
            "edit",
            &json!({"file_path": "dup.txt", "old_string": "x", "new_string": "y"}),
        );
        assert!(!o.ok && o.output.contains("3 times"), "{}", o.output);
        assert_eq!(fs::read_to_string(dir.join("dup.txt")).unwrap(), "x x x\n");

        // replace_all works
        let o = execute(
            &mut ctx,
            "edit",
            &json!({"file_path": "dup.txt", "old_string": "x", "new_string": "y", "replace_all": true}),
        );
        assert!(o.ok);
        assert_eq!(fs::read_to_string(dir.join("dup.txt")).unwrap(), "y y y\n");
    }

    #[test]
    fn multi_edit_is_atomic_on_failure() {
        let (mut ctx, dir) = proj();
        execute(&mut ctx, "read", &json!({"file_path": "src/main.rs"}));
        let o = execute(
            &mut ctx,
            "multi_edit",
            &json!({
                "file_path": "src/main.rs",
                "edits": [
                    {"old_string": "main", "new_string": "start"},
                    {"old_string": "NOT-PRESENT", "new_string": "?"}
                ]
            }),
        );
        assert!(!o.ok, "second edit missing -> whole call fails");
        assert!(
            fs::read_to_string(dir.join("src/main.rs"))
                .unwrap()
                .contains("fn main()"),
            "file must stay untouched"
        );

        // all-good case applies both
        let o = execute(
            &mut ctx,
            "multi_edit",
            &json!({
                "file_path": "src/main.rs",
                "edits": [
                    {"old_string": "main", "new_string": "start"},
                    {"old_string": "TODO", "new_string": "DONE"}
                ]
            }),
        );
        assert!(o.ok, "{}", o.output);
        assert_eq!(
            fs::read_to_string(dir.join("src/main.rs")).unwrap(),
            "fn start() {}\n// DONE\n"
        );
    }

    #[test]
    fn git_show_shows_commit_and_file_at_revision() {
        let (mut ctx, dir) = proj();
        std::process::Command::new("git")
            .current_dir(&dir)
            .args(["add", "."])
            .status()
            .unwrap();
        let first = execute(&mut ctx, "git_commit", &json!({"message": "init"}));
        assert!(first.ok, "{}", first.output);
        fs::write(dir.join("README.md"), "# changed\n").unwrap();
        std::process::Command::new("git")
            .current_dir(&dir)
            .args(["add", "."])
            .status()
            .unwrap();
        let second = execute(&mut ctx, "git_commit", &json!({"message": "second"}));
        assert!(second.ok, "{}", second.output);

        // a commit: message, stat and patch
        let show = execute(&mut ctx, "git_show", &json!({"commit": "HEAD~1"}));
        assert!(show.ok, "{}", show.output);
        assert!(show.output.contains("init"), "{}", show.output);
        // a file at a revision
        let file = execute(
            &mut ctx,
            "git_show",
            &json!({"commit": "HEAD~1", "path": "README.md"}),
        );
        assert!(file.ok, "{}", file.output);
        assert!(file.output.contains("# demo"), "{}", file.output);
        // host-owned state is not readable through git either
        let blocked = execute(&mut ctx, "git_show", &json!({"path": ".sqwai/plan.json"}));
        assert!(!blocked.ok, "{}", blocked.output);
    }

    #[test]
    fn git_tools_and_patch_work_in_project_root() {
        let (mut ctx, dir) = proj();
        let status = execute(&mut ctx, "git_status", &json!({}));
        assert!(status.ok, "{}", status.output);
        assert!(status.output.contains("##") || status.output.contains("No commits"));

        let diff = execute(&mut ctx, "git_diff", &json!({}));
        assert!(diff.ok, "{}", diff.output);

        let log = execute(&mut ctx, "git_log", &json!({"count": 1}));
        assert!(!log.ok);
        assert!(
            log.output.contains("does not have any commits"),
            "{}",
            log.output
        );

        let branches = execute(&mut ctx, "git_branch", &json!({"action": "current"}));
        assert!(branches.ok, "{}", branches.output);

        std::process::Command::new("git")
            .current_dir(&dir)
            .args(["add", "."])
            .status()
            .unwrap();
        let commit = execute(&mut ctx, "git_commit", &json!({"message": "init"}));
        assert!(commit.ok, "{}", commit.output);

        let log = execute(&mut ctx, "git_log", &json!({"count": 1}));
        assert!(log.ok, "{}", log.output);
        assert!(log.output.contains("init"), "{}", log.output);
        let patch = "diff --git a/README.md b/README.md\nindex 9daeafb..f3b0735 100644\n--- a/README.md\n+++ b/README.md\n@@ -1 +1 @@\n-# demo\n+# patched\n";
        let applied = execute(&mut ctx, "patch", &json!({"patch": patch}));
        assert!(applied.ok, "{}", applied.output);
        assert_eq!(
            fs::read_to_string(dir.join("README.md"))
                .unwrap()
                .replace("\r\n", "\n"),
            "# patched\n"
        );

        let rejected = execute(&mut ctx, "patch", &json!({"patch": "not a patch"}));
        assert!(!rejected.ok);
        assert_eq!(
            fs::read_to_string(dir.join("README.md"))
                .unwrap()
                .replace("\r\n", "\n"),
            "# patched\n"
        );
    }

    #[test]
    fn git_commit_requires_message() {
        let (mut ctx, _dir) = proj();
        let result = execute(&mut ctx, "git_commit", &json!({}));
        assert!(!result.ok);
        assert!(result.output.contains("non-empty message"));
    }

    #[test]
    fn git_stage_stages_untracked_files_and_supports_reset() {
        let (mut ctx, dir) = proj();
        // Initially clean initial commit
        let stage_init = execute(&mut ctx, "git_stage", &json!({"all": true}));
        assert!(stage_init.ok, "{}", stage_init.output);
        let commit_init = execute(&mut ctx, "git_commit", &json!({"message": "init"}));
        assert!(commit_init.ok, "{}", commit_init.output);

        // 1. Create untracked file and stage via paths
        fs::write(dir.join("created.txt"), "hello untracked\n").unwrap();
        let status_before = execute(&mut ctx, "git_status", &json!({}));
        assert!(status_before.output.contains("?? created.txt"));

        let stage_file = execute(&mut ctx, "git_stage", &json!({"paths": ["created.txt"]}));
        assert!(stage_file.ok, "{}", stage_file.output);
        let status_staged = execute(&mut ctx, "git_status", &json!({}));
        assert!(status_staged.output.contains("A  created.txt"));

        // 2. Unstage via reset
        let unstage = execute(
            &mut ctx,
            "git_stage",
            &json!({"action": "reset", "paths": ["created.txt"]}),
        );
        assert!(unstage.ok, "{}", unstage.output);
        let status_unstaged = execute(&mut ctx, "git_status", &json!({}));
        assert!(status_unstaged.output.contains("?? created.txt"));

        // 3. Stage via all: true
        let stage_all = execute(&mut ctx, "git_stage", &json!({"all": true}));
        assert!(stage_all.ok, "{}", stage_all.output);
        let commit = execute(
            &mut ctx,
            "git_commit",
            &json!({"message": "commit created"}),
        );
        assert!(commit.ok, "{}", commit.output);

        let status_clean = execute(&mut ctx, "git_status", &json!({}));
        assert!(!status_clean.output.contains("created.txt"));

        // 4. Validation errors
        let bad_action = execute(
            &mut ctx,
            "git_stage",
            &json!({"action": "invalid", "all": true}),
        );
        assert!(!bad_action.ok);
        assert!(bad_action.output.contains("action must be add or reset"));

        let no_args = execute(&mut ctx, "git_stage", &json!({}));
        assert!(!no_args.ok);
        assert!(
            no_args
                .output
                .contains("requires either all: true or non-empty paths")
        );

        let forbidden_sqwai = execute(
            &mut ctx,
            "git_stage",
            &json!({"paths": [".sqwai/something"]}),
        );
        assert!(!forbidden_sqwai.ok);
        assert!(forbidden_sqwai.output.contains("host-owned state"));

        let bad_path = execute(&mut ctx, "git_stage", &json!({"paths": ["../outside"]}));
        assert!(!bad_path.ok);
        assert!(bad_path.output.contains("bad path"));
    }
    #[test]
    fn tool_specs_are_stably_sorted() {
        let names: Vec<String> = tool_specs(false).iter().map(|t| t.name.clone()).collect();
        assert_eq!(names, tool_names());

        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "tool order must not depend on registration");
        let again: Vec<String> = tool_specs(false).iter().map(|t| t.name.clone()).collect();
        assert_eq!(names, again, "the schema block must be byte-stable");
    }

    /// MCP servers answer in their own order: merging must re-sort the whole
    /// set, or the tool prefix (and the cache behind it) re-keys whenever a
    /// server flakes or reorders.
    #[test]
    fn merge_specs_sorts_external_tools_with_builtin_ones() {
        let spec = |name: &str| crate::providers::ToolSpec {
            name: name.into(),
            description: "d".into(),
            parameters: serde_json::json!({"type": "object"}),
        };
        let merged = merge_specs(
            vec![spec("read"), spec("write")],
            &[spec("zzz"), spec("aaa")],
        );
        let names: Vec<String> = merged.iter().map(|t| t.name.clone()).collect();
        assert_eq!(names, vec!["aaa", "read", "write", "zzz"]);
        // server order must not leak through: reversed input, same output
        let merged = merge_specs(
            vec![spec("read"), spec("write")],
            &[spec("aaa"), spec("zzz")],
        );
        let names: Vec<String> = merged.iter().map(|t| t.name.clone()).collect();
        assert_eq!(names, vec!["aaa", "read", "write", "zzz"]);
    }

    /// Mid-trim keeps head+tail around the marker, never splits a codepoint,
    /// and leaves fitting text untouched.
    #[test]
    fn trim_middle_keeps_head_and_tail() {
        assert_eq!(trim_middle("short", 10), "short");
        let out = trim_middle(&"€".repeat(100), 9);
        assert!(out.contains("output truncated"), "{out}");
        assert!(out.starts_with("€€€"), "head: {out}");
        assert!(out.ends_with("€€€€€€"), "tail: {out}");
        assert!(out.chars().count() <= 9 + 60, "{out}");
    }

    /// One schema set in every mode: the tool block is part of the request
    /// prefix, so a mode-dependent set re-keys the cache on every Plan/Act
    /// toggle. Plan mode refuses mutating calls at dispatch instead
    /// (`is_mutating_call`); the schemas stay identical so the prefix does.
    #[test]
    fn tool_schemas_are_mode_independent() {
        let names = |plan_mode: bool| {
            let mut names: Vec<String> = tool_specs(plan_mode)
                .iter()
                .map(|t| t.name.clone())
                .collect();
            names.sort();
            names
        };
        assert_eq!(names(true), names(false));
        let specs = tool_specs(true);
        for name in ["read", "plan", "write", "edit", "bash"] {
            assert!(
                specs.iter().any(|t| t.name == name),
                "{name} is advertised in PLAN mode too"
            );
        }
        // the refusal lives at dispatch, not in the schemas
        assert!(is_mutating_call(
            "write",
            &json!({"file_path": "src/a.rs", "content": "x"})
        ));
        assert!(!is_mutating_call("read", &json!({"file_path": "src/a.rs"})));
    }

    /// §2.1.2 makes the plan budget a host value: model context times
    /// [plan].budget_ratio. It used to be read out of the model's own tool
    /// arguments — `args["context_limit"]` — so the model could raise its own
    /// ceiling and skip the folding in §2.1.5. The field is no longer even
    /// advertised.
    #[test]
    fn the_plan_budget_comes_from_the_host_not_the_model() {
        let (mut ctx, dir) = proj();
        ctx = ctx.with_plan_limits(
            crate::config::PlanConfig {
                budget_ratio: 0.10,
                ..Default::default()
            },
            200_000,
        );

        // the model asks for a huge budget in its arguments; it is ignored
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "budget from the host",
                "criteria": ["note"],
                "steps": [{"title": "one"}],
                "context_limit": 100_000_000u64,
            }),
        );
        assert!(created.ok, "{}", created.output);
        let plan = plan::open_active(&dir).unwrap().unwrap();
        assert_eq!(plan.budget.limit, 20_000, "0.10 of a 200k context");

        assert!(
            !tool_specs(false)
                .iter()
                .find(|spec| spec.name == "plan")
                .unwrap()
                .parameters["properties"]
                .as_object()
                .unwrap()
                .contains_key("context_limit"),
            "the model is not asked for its own context any more"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// A tiny context still leaves room for a plan rather than a budget of
    /// zero, which would fold everything on the first injection.
    #[test]
    fn the_plan_budget_has_a_floor() {
        let (mut ctx, dir) = proj();
        ctx = ctx.with_plan_limits(crate::config::PlanConfig::default(), 100);
        assert!(
            plan_op(
                &mut ctx,
                &json!({"op": "create", "goal": "tiny context", "criteria": ["note"],
                        "steps": [{"title": "one"}]}),
            )
            .ok
        );
        assert_eq!(
            plan::open_active(&dir).unwrap().unwrap().budget.limit,
            MIN_PLAN_BUDGET_TOKENS
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// [plan].max_steps is the host's limit, not a constant.
    #[test]
    fn max_steps_comes_from_the_config() {
        let (mut ctx, dir) = proj();
        ctx = ctx.with_plan_limits(
            crate::config::PlanConfig {
                max_steps: 2,
                ..Default::default()
            },
            100_000,
        );
        let steps: Vec<_> = (0..3)
            .map(|i| json!({"title": format!("step {i}")}))
            .collect();
        let refused = plan_op(
            &mut ctx,
            &json!({"op": "create", "goal": "over the limit", "criteria": ["note"], "steps": steps}),
        );
        assert!(!refused.ok, "{}", refused.output);
        assert!(
            refused.output.contains("too_many_steps") || refused.output.contains("2"),
            "{}",
            refused.output
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// `git_branch` advertises every action in every mode; mutating ones
    /// are refused at dispatch. Cache-stable schemas beat saving the model
    /// a refusal turn.
    #[test]
    fn git_branch_actions_are_identical_across_modes() {
        let actions = |plan_mode: bool| {
            tool_specs(plan_mode)
                .into_iter()
                .find(|spec| spec.name == "git_branch")
                .expect("git_branch stays available for inspection")
                .parameters["properties"]["action"]["enum"]
                .clone()
        };
        assert_eq!(
            actions(true),
            json!(["list", "current", "create", "switch"])
        );
        assert_eq!(
            actions(false),
            json!(["list", "current", "create", "switch"])
        );
        assert!(is_mutating_call("git_branch", &json!({"action": "create"})));
        assert!(!is_mutating_call("git_branch", &json!({"action": "list"})));
    }

    /// §4: the guard is hash-tracked, so a file changed by `bash` since the
    /// last read has to be read again. With paths alone the model could edit
    /// content that was already gone.
    #[test]
    fn a_file_changed_after_reading_it_must_be_read_again() {
        let (mut ctx, dir) = proj();
        assert!(execute(&mut ctx, "read", &json!({"file_path": "src/main.rs"})).ok);

        // something else changes it: bash, a formatter, the user's editor
        fs::write(dir.join("src/main.rs"), "fn main() { /* moved on */ }\n").unwrap();

        let refused = execute(
            &mut ctx,
            "edit",
            &json!({"file_path": "src/main.rs", "old_string": "fn main() {}", "new_string": "x"}),
        );
        assert!(!refused.ok, "{}", refused.output);
        assert!(
            refused.output.contains("changed since you read it"),
            "{}",
            refused.output
        );

        // reading again clears it
        assert!(execute(&mut ctx, "read", &json!({"file_path": "src/main.rs"})).ok);
        let accepted = execute(
            &mut ctx,
            "edit",
            &json!({"file_path": "src/main.rs", "old_string": "moved on", "new_string": "here"}),
        );
        assert!(accepted.ok, "{}", accepted.output);
        fs::remove_dir_all(&dir).ok();
    }

    /// The guard is keyed on the canonical path, so the same file spelled two
    /// ways is the same file. It used to key on the string the model passed,
    /// which refused a legitimate edit as unread.
    #[test]
    fn the_read_guard_does_not_care_how_the_path_is_spelled() {
        let (mut ctx, dir) = proj();
        assert!(execute(&mut ctx, "read", &json!({"file_path": "src/main.rs"})).ok);
        let accepted = execute(
            &mut ctx,
            "edit",
            &json!({"file_path": "./src/main.rs", "old_string": "fn main() {}", "new_string": "fn main() { }"}),
        );
        assert!(accepted.ok, "{}", accepted.output);
        fs::remove_dir_all(&dir).ok();
    }

    /// §2.1.4 lets a step close on "diagnostics with zero errors". The
    /// record was defined in §2.2.2 and never written, so that branch was
    /// unreachable — this is the evidence path, now that it exists.
    #[test]
    fn clean_diagnostics_close_a_verify_step() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({"op": "create", "goal": "diagnostics as evidence", "criteria": ["note"],
                    "steps": [{"title": "check", "kind": "verify"}]}),
        );
        assert!(created.ok, "{}", created.output);
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);

        let mut journal = crate::agent::journal::Journal::open(&dir, &ctx.session_id).unwrap();
        journal.set_attribution(Some("1".into()), Some(plan_id), "main");
        journal
            .append("plan", json!({"op": "start", "id": "1"}))
            .unwrap();
        journal
            .append_evidence(
                "diagnostics",
                json!({"path": "src/main.rs", "errors": 0, "warnings": 2, "server": "rust-analyzer"}),
            )
            .unwrap();

        let finished = plan_op(
            &mut ctx,
            &json!({"op": "finish", "id": "1", "summary": "no errors reported"}),
        );
        assert!(finished.ok, "{}", finished.output);
        fs::remove_dir_all(&dir).ok();
    }

    /// `join` is host-only: the model cannot add sessions to a plan, not
    /// even its own — membership comes from `plan start` and child spawns.
    #[test]
    fn plan_join_op_is_refused_for_the_model() {
        let (mut ctx, _dir) = proj();
        let refused = plan_op(&mut ctx, &json!({"op": "join", "session": "sub-1"}));
        assert!(!refused.ok, "join must never run from the model");
        assert!(refused.output.contains("host-only"), "{}", refused.output);
    }
    /// A subagent mutating after its step was reopened would attach stale
    /// work to a fresh epoch (§2.2.4). The dispatcher refuses the mutation
    /// instead; read-only tools keep working, and a fresh spawn proceeds.
    #[test]
    fn subagent_mutation_refused_after_step_reopen() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({"op": "create", "goal": "guarded work", "criteria": ["note"],
                    "steps": [{"title": "change things"}]}),
        );
        assert!(created.ok, "{}", created.output);
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        ctx.current_step = Some("1".into());
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);

        let mut child = ToolCtx::new(&dir).in_session("sub-1");
        child.subagent_step = Some(plan::StepContext {
            plan_id: plan_id.clone(),
            step_id: "1".into(),
            step_epoch: 0,
        });
        let wrote = execute(
            &mut child,
            "write",
            &json!({"file_path": "src/child.rs", "content": "fresh work\n"}),
        );
        assert!(wrote.ok, "{}", wrote.output);

        // The step is done and reopened: epoch moves to 1.
        let mut active = plan::open_active(&dir).unwrap().unwrap();
        active.steps[0].status = plan::StepStatus::Done;
        plan::store(&dir, &active).unwrap();
        plan::reopen_for_undo(&mut active, "1", "test reopen").unwrap();
        plan::store(&dir, &active).unwrap();

        let refused = execute(
            &mut child,
            "write",
            &json!({"file_path": "src/child2.rs", "content": "stale work\n"}),
        );
        assert!(!refused.ok, "stale mutation must be refused");
        assert!(refused.output.contains("stale_epoch"), "{}", refused.output);

        // Read-only observation is not a mutation: still allowed.
        let read = execute(&mut child, "read", &json!({"file_path": "src/main.rs"}));
        assert!(read.ok, "{}", read.output);

        // A fresh spawn inheriting epoch 1 proceeds normally.
        child.subagent_step = Some(plan::StepContext {
            plan_id,
            step_id: "1".into(),
            step_epoch: 1,
        });
        let wrote_again = execute(
            &mut child,
            "write",
            &json!({"file_path": "src/child3.rs", "content": "fresh work\n"}),
        );
        assert!(wrote_again.ok, "{}", wrote_again.output);
        fs::remove_dir_all(&dir).ok();
    }

    /// S1 writer lock: while an undo restore holds the lock, file
    /// mutations are refused with `code: writer_locked` instead of
    /// landing under the revert.
    #[test]
    fn writer_lock_refuses_mutations_during_restore() {
        let (mut ctx, dir) = proj();
        let _restore = crate::agent::undo_guard::hold_restore();
        let refused = execute(
            &mut ctx,
            "write",
            &json!({"file_path": "src/locked.rs", "content": "nope\n"}),
        );
        assert!(!refused.ok, "locked mutation must be refused");
        assert!(
            refused.output.contains("writer_locked"),
            "{}",
            refused.output
        );
        drop(_restore);
        let wrote = execute(
            &mut ctx,
            "write",
            &json!({"file_path": "src/locked.rs", "content": "ok\n"}),
        );
        assert!(wrote.ok, "{}", wrote.output);
        fs::remove_dir_all(&dir).ok();
    }

    /// `plan abandon` applies directly now (no confirm dialog): a quoted
    /// reason retires the plan; a thin reason is refused with a code+hint.
    #[test]
    fn plan_abandon_applies_with_a_reason() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({"op":"create","goal":"g","criteria":["note"],"steps":[{"title":"one"}]}),
        );
        assert!(created.ok, "{}", created.output);
        let thin = plan_op(&mut ctx, &json!({"op": "abandon", "reason": "nope"}));
        assert!(!thin.ok, "thin reason must be refused: {}", thin.output);
        assert!(thin.output.contains("thin_reason"), "{}", thin.output);
        let out = plan_op(
            &mut ctx,
            &json!({"op": "abandon", "reason": "goal targets removed feature X, steps assume the deleted API"}),
        );
        assert!(out.ok, "{}", out.output);
        assert!(
            plan::open_active(&dir).unwrap().is_none(),
            "abandoned plan is no longer active"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// Plan-lite grows teeth: criteria appended after create land pending,
    /// Done-criteria notes ride create, show in the render, and never gate
    /// complete: they are the agent's own reminders, not checks to settle.
    #[test]
    fn plan_criteria_are_visible_and_non_blocking() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "lite",
                "constraints": [],
                "criteria": ["eyeball the diff", "ask Anna about scope"],
                "steps": [{"title": "s"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        // `show` is not a model tool (the live plan rides every request):
        // the dispatcher refuses, state is read from the plan file instead
        let shown = plan_op(&mut ctx, &json!({"op": "show"}));
        assert!(!shown.ok);
        assert!(
            shown.output.contains("no plan show tool"),
            "{}",
            shown.output
        );
        let plan = plan::open_active(&dir).unwrap().unwrap();
        let rendered = plan::render(&plan);
        assert!(rendered.contains("ask Anna about scope"), "{rendered}");
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        assert!(
            plan_op(
                &mut ctx,
                &json!({"op": "finish", "id": "1", "summary": "did it"})
            )
            .ok
        );
        let completed = plan_op(&mut ctx, &json!({"op": "complete"}));
        assert!(
            completed.ok,
            "criteria must not block: {}",
            completed.output
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn plan_ops_round_trip_through_the_dispatcher() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "wire the plan tool",
                "constraints": ["no new dependencies"],
                "acceptance": ["cmd: cargo test"],
                "steps": [
                    {"title": "add the schema", "kind": "research"},
                    {"title": "add the dispatcher"}
                ]
            }),
        );
        assert!(created.ok, "{}", created.output);
        assert!(
            created.output.contains("created with 2 steps"),
            "{}",
            created.output
        );

        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        // the writer session must be the agent's own (#171): a foreign
        // journal file no longer attaches evidence to this plan
        let mut evidence_journal =
            crate::agent::journal::Journal::open(&dir, &ctx.session_id).unwrap();
        evidence_journal.set_attribution(Some("1".into()), Some(plan_id.clone()), "main");
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        evidence_journal
            .append("plan", json!({"op": "start"}))
            .unwrap();
        evidence_journal
            .append_evidence("tool_result", json!({"tool": "read", "ok": true}))
            .unwrap();
        let finish = plan_op(
            &mut ctx,
            &json!({"op": "finish", "id": "1", "summary": "schema added", "evidence": [2]}),
        );
        assert!(finish.ok, "{}", finish.output);
        assert!(
            !finish.output.contains("warning:"),
            "finish output should not contain warning: {}",
            finish.output
        );

        let shown = plan_op(&mut ctx, &json!({"op": "show"}));
        assert!(!shown.ok, "show is not a model tool: {}", shown.output);
        let plan = plan::open_active(&dir).unwrap().unwrap();
        let rendered = plan::render(&plan);
        assert!(rendered.contains("goal: wire the plan tool"), "{rendered}");
        assert!(
            rendered.contains("[x] 1"),
            "step 1 should read as done:\n{rendered}"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn finish_warns_when_evidence_predates_step_start() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "boundary test",
                "criteria": ["note"],
                "steps": [{"title": "step 1", "kind": "research"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        let mut journal = crate::agent::journal::Journal::open(&dir, "boundary-test").unwrap();

        // Premature evidence before start (seq 1)
        journal.set_attribution(Some("1".into()), Some(plan_id.clone()), "main");
        let premature_seq = journal
            .append("tool_result", json!({"tool": "read", "ok": true}))
            .unwrap();
        assert_eq!(premature_seq, 1);

        // Op start (seq 2)
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        journal
            .append("plan", json!({"op": "start", "id": "1"}))
            .unwrap();

        // Premature evidence is in step.evidence
        let mut plan = plan::open_active(&dir).unwrap().unwrap();
        plan.step_mut("1")
            .unwrap()
            .evidence
            .push(crate::plan::EvidenceRef {
                session: "boundary-test".into(),
                seq: premature_seq,
            });
        plan::store(&dir, &plan).unwrap();

        // Finishing with premature evidence triggers warning
        let finish = plan_op(
            &mut ctx,
            &json!({"op": "finish", "id": "1", "summary": "researched"}),
        );
        assert!(finish.ok, "finish still succeeds: {}", finish.output);
        assert!(
            finish.output.contains("warning:") && finish.output.contains("predates step 1 start"),
            "stale evidence must produce warning: {}",
            finish.output
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn memory_read_returns_only_a_valid_diary_date() {
        let (mut ctx, dir) = proj();
        let path = crate::agent::diary::diary_path(
            &dir,
            chrono::NaiveDate::from_ymd_opt(2026, 9, 4).unwrap(),
        );
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "## diary\n- fact\n").unwrap();
        let read = execute(&mut ctx, "memory_read", &json!({"date": "2026-09-04"}));
        assert!(read.ok, "{}", read.output);
        assert!(read.output.contains("fact"));
        let invalid = execute(&mut ctx, "memory_read", &json!({"date": "../secret"}));
        assert!(!invalid.ok);
        assert!(invalid.output.contains("YYYY-MM-DD"), "{}", invalid.output);
        fs::remove_dir_all(&dir).ok();
    }

    /// §2.1.4's closure moment: finishing a step with an open assumption
    /// succeeds and says so. A silent finish is how an assumption outlives the
    /// work that depended on it.
    #[test]
    fn finishing_a_step_warns_about_its_open_assumptions() {
        let (mut ctx, dir) = proj();
        let created = execute(
            &mut ctx,
            "plan",
            &json!({
                "op": "create",
                "criteria": ["note"],
                "goal": "close the assumption loop",
                "steps": [{"title": "make the change", "kind": "change"}],
            }),
        );
        assert!(created.ok, "{}", created.output);
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);

        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        let mut journal = crate::agent::journal::Journal::open(&dir, &ctx.session_id).unwrap();
        journal.set_attribution(Some("1".into()), Some(plan_id), "main");
        let seq = journal
            .append(
                "note",
                json!({"by": "model", "note": "assumption", "text": "the config key is stable"}),
            )
            .unwrap();
        // evidence for the step, so `finish` is not rejected for that
        journal
            .append_evidence("file_diff", json!({"path": "src/main.rs"}))
            .unwrap();

        let finished = plan_op(
            &mut ctx,
            &json!({"op": "finish", "id": "1", "summary": "changed it"}),
        );
        assert!(
            finished.ok,
            "the warning must not block: {}",
            finished.output
        );
        assert!(
            finished.output.contains("open assumption")
                && finished.output.contains(&format!("j#{seq}")),
            "{}",
            finished.output
        );

        // and a note that closes it is accepted, while a bogus target is not
        let bogus = execute(
            &mut ctx,
            "note",
            &json!({"note": "nothing to close", "kind": "assumption", "resolves": 9999}),
        );
        assert!(!bogus.ok, "{}", bogus.output);
        assert!(
            bogus.output.contains("not an open assumption"),
            "{}",
            bogus.output
        );

        let closing = execute(
            &mut ctx,
            "note",
            &json!({"note": "verified against the config", "kind": "assumption", "resolves": seq}),
        );
        assert!(closing.ok, "{}", closing.output);
        assert!(closing.output.contains(&format!("resolves j#{seq}")));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn note_requires_an_allowed_kind_and_non_empty_text() {
        let (mut ctx, dir) = proj();
        let missing = execute(&mut ctx, "note", &json!({"note": "", "kind": "lesson"}));
        assert!(!missing.ok);
        assert!(missing.output.contains("1-2000"), "{}", missing.output);
        let invalid = execute(
            &mut ctx,
            "note",
            &json!({"note": "keep this", "kind": "other"}),
        );
        assert!(!invalid.ok);
        assert!(invalid.output.contains("invalid"), "{}", invalid.output);
        let accepted = execute(
            &mut ctx,
            "note",
            &json!({"note": "keep this", "kind": "decision"}),
        );
        assert!(accepted.ok, "{}", accepted.output);
        fs::remove_dir_all(&dir).ok();
    }

    /// Steps close on a summary alone — the host records what it observed
    /// and never demands an oath to close progress.
    #[test]
    fn soft_finish_closes_on_summary_alone() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "criteria": ["note"],
                "goal": "soft close",
                "steps": [{"title": "think"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        // no tool calls, no journal evidence at all
        let done = plan_op(
            &mut ctx,
            &json!({"op": "finish", "id": "1", "summary": "thought about it"}),
        );
        assert!(done.ok, "{}", done.output);
        assert_eq!(
            plan::open_active(&dir).unwrap().unwrap().steps[0].status,
            plan::StepStatus::Done
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// #171: `plan start` on the project's active plan is an explicit
    /// adoption — the session joins (membership recorded) instead of
    /// failing or silently borrowing foreign work. Other ops stay strict.
    #[test]
    fn plan_start_joins_a_foreign_active_plan() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "criteria": ["note"],
                "goal": "shared work",
                "steps": [{"title": "step one"}]
            }),
        );
        assert!(created.ok, "{}", created.output);

        // another session, no plan of its own
        let mut other = ToolCtx::new(&dir).in_session("other-sess");
        let started = plan_op(&mut other, &json!({"op": "start", "id": "1"}));
        assert!(
            started.ok,
            "start must join, not refuse: {}",
            started.output
        );

        let plan = plan::open_active(&dir).unwrap().unwrap();
        assert!(
            plan.sessions.contains(&ctx.session_id)
                && plan.sessions.contains(&"other-sess".to_string()),
            "both sessions are members: {:?}",
            plan.sessions
        );

        // ...while a non-start op from the outsider still resolves nothing
        let mut third = ToolCtx::new(&dir).in_session("third-sess");
        let shown = plan_op(&mut third, &json!({"op": "show"}));
        assert!(!shown.ok, "reads stay session-strict too: {}", shown.output);
        let finished = plan_op(
            &mut third,
            &json!({"op": "finish", "id": "1", "summary": "mine"}),
        );
        assert!(
            !finished.ok,
            "finish without membership must not touch the step"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// Call rows show whether the model waited: `job 1` vs `job 1 wait 60s`.
    #[test]
    fn bash_output_summary_shows_wait_params() {
        assert_eq!(call_summary("bash_output", &json!({"id": 1})), "job 1");
        assert_eq!(
            call_summary("bash_output", &json!({"id": 1, "wait_secs": 60})),
            "job 1 wait 60s"
        );
        assert_eq!(
            call_summary("bash_output", &json!({"id": 2, "from_start": true})),
            "job 2 from start"
        );
    }

    /// The one create-time check: a plan must carry at least one done-note.
    /// Free text is fine (no typing gate); only emptiness is refused.
    #[test]
    fn create_refuses_empty_criteria() {
        let (mut ctx, dir) = proj();
        let rejected = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "no notes",
                "criteria": [],
                "steps": [{"title": "verify"}]
            }),
        );
        assert!(!rejected.ok, "{}", rejected.output);
        assert!(
            rejected.output.contains("empty_criteria"),
            "{}",
            rejected.output
        );
        // and nothing was stored
        assert!(plan::open_active(&dir).unwrap().is_none());
        fs::remove_dir_all(&dir).ok();
    }

    /// Surrender through the dispatcher: journal-first, terminal, quoted.
    /// A blocked plan refuses further work like every closed plan.
    #[test]
    fn block_plan_surrenders_with_a_quote() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "impossible task",
                "acceptance": ["manual: spec holds"],
                "steps": [{"title": "try"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;

        let blocked = plan_op(
            &mut ctx,
            &json!({"op": "block_plan", "reason": "spec says 404, test expects 200"}),
        );
        assert!(blocked.ok, "{}", blocked.output);
        let plan = plan::read_plan_file(&dir, &plan_id).expect("blocked plan reads back");
        assert_eq!(plan.status, plan::PlanStatus::Blocked);
        assert_eq!(
            plan.blocked_reason.as_deref(),
            Some("spec says 404, test expects 200")
        );
        assert!(plan::render(&plan).contains("spec says 404"));

        // terminal: nothing resolves as active anymore, so the dispatcher
        // guides toward a new plan (the plan_closed guard below it covers
        // direct apply callers)
        let start = plan_op(&mut ctx, &json!({"op": "start", "id": "1"}));
        assert!(!start.ok, "{}", start.output);
        assert!(start.output.contains("no active plan"), "{}", start.output);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn plan_rejections_carry_a_code_and_a_hint() {
        let (mut ctx, dir) = proj();
        plan_op(
            &mut ctx,
            &json!({"op": "create", "goal": "g", "criteria": ["note"], "steps": [{"title": "one"}]}),
        );
        // finishing a step that was never started
        let bad = plan_op(
            &mut ctx,
            &json!({"op": "finish", "id": "1", "summary": "x"}),
        );
        assert!(!bad.ok);
        assert!(
            bad.output.contains("step_not_in_progress"),
            "{}",
            bad.output
        );
        assert!(bad.output.contains("hint"), "{}", bad.output);
        // a second create is refused while one is active
        let second = plan_op(
            &mut ctx,
            &json!({"op": "create", "goal": "h", "criteria": ["note"], "steps": [{"title": "two"}]}),
        );
        assert!(!second.ok);
        assert!(second.output.contains("plan_exists"), "{}", second.output);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn outline_extracts_tree_sitter_declarations() {
        let dir = tempfile::tempdir().unwrap();
        let rs_code = r#"
pub struct ServerConfig {
    pub port: u16,
}

impl ServerConfig {
    pub fn new(port: u16) -> Self {
        Self { port }
    }
}

pub enum State {
    Running,
    Stopped,
}

pub async fn start_server() -> Result<(), ()> {
    Ok(())
}
"#;
        fs::write(dir.path().join("server.rs"), rs_code).unwrap();

        let py_code = r#"
class Worker:
    def __init__(self, name: str):
        self.name = name

    def run(self) -> None:
        pass

def main():
    w = Worker("job")
"#;
        fs::write(dir.path().join("worker.py"), py_code).unwrap();

        let mut ctx = ToolCtx::new(dir.path());

        let res_rs = execute(&mut ctx, "outline", &json!({"path": "server.rs"}));
        assert!(res_rs.ok, "{}", res_rs.output);
        assert!(
            res_rs.output.contains("pub struct ServerConfig"),
            "{}",
            res_rs.output
        );
        assert!(
            res_rs.output.contains("impl ServerConfig"),
            "{}",
            res_rs.output
        );
        assert!(
            res_rs.output.contains("pub fn new(port: u16) -> Self"),
            "{}",
            res_rs.output
        );
        assert!(
            res_rs.output.contains("pub enum State"),
            "{}",
            res_rs.output
        );
        assert!(res_rs.output.contains("Running"), "{}", res_rs.output);
        assert!(
            res_rs
                .output
                .contains("pub async fn start_server() -> Result<(), ()>"),
            "{}",
            res_rs.output
        );

        let res_py = execute(&mut ctx, "outline", &json!({"path": "worker.py"}));
        assert!(res_py.ok, "{}", res_py.output);
        assert!(res_py.output.contains("class Worker:"), "{}", res_py.output);
        assert!(
            res_py.output.contains("def __init__(self, name: str):"),
            "{}",
            res_py.output
        );
        assert!(
            res_py.output.contains("def run(self) -> None:"),
            "{}",
            res_py.output
        );
        assert!(res_py.output.contains("def main():"), "{}", res_py.output);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn outline_depth_filtering_and_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let rs_code = r#"
struct Outer {
}

impl Outer {
    fn inner_method() {}
}
"#;
        fs::write(dir.path().join("test.rs"), rs_code).unwrap();

        let rb_code = r#"
module Analytics
  class Tracker
    def track_event(name)
      puts name
    end
  end
end
"#;
        fs::write(dir.path().join("tracker.rb"), rb_code).unwrap();

        let md_code = r#"
# Project Documentation
## Getting Started
### Prerequisites
"#;
        fs::write(dir.path().join("README.md"), md_code).unwrap();

        let mut ctx = ToolCtx::new(dir.path());

        let res_d1 = execute(
            &mut ctx,
            "outline",
            &json!({"path": "test.rs", "max_depth": 1}),
        );
        assert!(res_d1.ok, "{}", res_d1.output);
        assert!(res_d1.output.contains("struct Outer"), "{}", res_d1.output);
        assert!(!res_d1.output.contains("inner_method"), "{}", res_d1.output);

        let res_d2 = execute(
            &mut ctx,
            "outline",
            &json!({"path": "test.rs", "max_depth": 2}),
        );
        assert!(res_d2.ok, "{}", res_d2.output);
        assert!(
            res_d2.output.contains("fn inner_method()"),
            "{}",
            res_d2.output
        );

        let res_rb = execute(
            &mut ctx,
            "outline",
            &json!({"path": "tracker.rb", "max_depth": 3}),
        );
        assert!(res_rb.ok, "{}", res_rb.output);
        assert!(
            res_rb.output.contains("module Analytics"),
            "{}",
            res_rb.output
        );
        assert!(res_rb.output.contains("class Tracker"), "{}", res_rb.output);
        assert!(
            res_rb.output.contains("def track_event(name)"),
            "{}",
            res_rb.output
        );

        let res_md = execute(
            &mut ctx,
            "outline",
            &json!({"path": "README.md", "max_depth": 2}),
        );
        assert!(res_md.ok, "{}", res_md.output);
        assert!(
            res_md.output.contains("# Project Documentation"),
            "{}",
            res_md.output
        );
        assert!(
            res_md.output.contains("## Getting Started"),
            "{}",
            res_md.output
        );
        assert!(
            !res_md.output.contains("### Prerequisites"),
            "{}",
            res_md.output
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn outline_validates_path_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolCtx::new(dir.path());

        let missing = execute(&mut ctx, "outline", &json!({}));
        assert!(!missing.ok);
        assert!(missing.output.contains("requires a 'path'"));

        let not_found = execute(&mut ctx, "outline", &json!({"path": "nonexistent.rs"}));
        assert!(!not_found.ok);
        assert!(not_found.output.contains("file not found"));

        let is_dir = execute(&mut ctx, "outline", &json!({"path": "."}));
        assert!(!is_dir.ok);
        assert!(is_dir.output.contains("found a directory"));

        let escape = execute(&mut ctx, "outline", &json!({"path": "../../etc/passwd"}));
        assert!(!escape.ok);
        assert!(escape.output.contains("escapes the project directory"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn outline_supports_c_cpp_csharp_java_go_and_ts() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("main.c"),
            "int calculate(int x) {\n    return x * 2;\n}\nint main() {\n    return calculate(5);\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("service.cpp"),
            "class Engine {\npublic:\n    void start() {}\n};\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("App.cs"),
            "namespace Demo {\n    class Greeter {\n        void SayHello() {}\n    }\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("Hello.java"),
            "class Hello {\n    void greet() {}\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("server.go"),
            "package main\n\ntype Server struct {}\n\nfunc (s *Server) Start() error {\n    return nil\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("index.ts"),
            "export interface Config {\n    port: number;\n}\n\nexport class App {\n    start(): void {}\n}\n",
        )
        .unwrap();

        let mut ctx = ToolCtx::new(dir.path());

        let c_res = execute(&mut ctx, "outline", &json!({"path": "main.c"}));
        assert!(c_res.ok, "{}", c_res.output);
        assert!(
            c_res.output.contains("int calculate(int x)"),
            "{}",
            c_res.output
        );
        assert!(c_res.output.contains("int main()"), "{}", c_res.output);

        let cpp_res = execute(
            &mut ctx,
            "outline",
            &json!({"path": "service.cpp", "max_depth": 2}),
        );
        assert!(cpp_res.ok, "{}", cpp_res.output);
        assert!(
            cpp_res.output.contains("class Engine"),
            "{}",
            cpp_res.output
        );
        assert!(
            cpp_res.output.contains("void start()"),
            "{}",
            cpp_res.output
        );

        let cs_res = execute(
            &mut ctx,
            "outline",
            &json!({"path": "App.cs", "max_depth": 2}),
        );
        assert!(cs_res.ok, "{}", cs_res.output);
        assert!(cs_res.output.contains("class Greeter"), "{}", cs_res.output);
        assert!(
            cs_res.output.contains("void SayHello()"),
            "{}",
            cs_res.output
        );

        let java_res = execute(
            &mut ctx,
            "outline",
            &json!({"path": "Hello.java", "max_depth": 2}),
        );
        assert!(java_res.ok, "{}", java_res.output);
        assert!(
            java_res.output.contains("class Hello"),
            "{}",
            java_res.output
        );
        assert!(
            java_res.output.contains("void greet()"),
            "{}",
            java_res.output
        );

        let go_res = execute(
            &mut ctx,
            "outline",
            &json!({"path": "server.go", "max_depth": 2}),
        );
        assert!(go_res.ok, "{}", go_res.output);
        assert!(
            go_res.output.contains("type Server struct"),
            "{}",
            go_res.output
        );
        assert!(
            go_res.output.contains("func (s *Server) Start() error"),
            "{}",
            go_res.output
        );

        let ts_res = execute(
            &mut ctx,
            "outline",
            &json!({"path": "index.ts", "max_depth": 2}),
        );
        assert!(ts_res.ok, "{}", ts_res.output);
        assert!(
            ts_res.output.contains("export interface Config"),
            "{}",
            ts_res.output
        );
        assert!(
            ts_res.output.contains("export class App"),
            "{}",
            ts_res.output
        );
        assert!(ts_res.output.contains("start(): void"), "{}", ts_res.output);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn glob_grep_ls_work() {
        let (mut ctx, _dir) = proj();
        let o = execute(&mut ctx, "glob", &json!({"pattern": "**/*.rs"}));
        assert!(o.ok && o.output.contains("src/main.rs"), "{}", o.output);

        let o = execute(
            &mut ctx,
            "grep",
            &json!({"pattern": "TODO", "include": "*.rs"}),
        );
        assert!(o.ok && o.output.contains("src/main.rs:2"), "{}", o.output);

        let o = execute(&mut ctx, "ls", &json!({"path": "src"}));
        assert!(o.ok && o.output.contains("main.rs"), "{}", o.output);
    }

    /// write a raw journal file with controlled timestamps
    fn write_journal(dir: &Path, session: &str, lines: &[String]) {
        let journal_dir = dir.join(".sqwai").join("journal");
        fs::create_dir_all(&journal_dir).unwrap();
        fs::write(
            journal_dir.join(format!("{session}.jsonl")),
            lines.join("\n") + "\n",
        )
        .unwrap();
    }

    fn rec(seq: u64, ts: &str, kind: &str, extra: &str) -> String {
        format!(
            r#"{{"seq":{seq},"ts":"{ts}","step":null,"plan":null,"agent":"main","kind":"{kind}"{extra}}}"#
        )
    }

    #[test]
    fn journal_read_renders_and_filters() {
        let (mut ctx, dir) = proj();
        write_journal(
            &dir,
            "shared",
            &[
                rec(1, "2026-01-01T10:00:00+00:00", "user_msg", r#","chars":42"#),
                rec(
                    2,
                    "2026-02-01T10:00:00+00:00",
                    "file_diff",
                    r#","path":"src/main.rs","added":3,"removed":1"#,
                ),
                rec(
                    3,
                    "2026-03-01T10:00:00+00:00",
                    "tool_result",
                    r#","tool":"bash","ok":false,"code":"cancelled""#,
                ),
            ],
        );

        // default: chronological tail with a header
        let o = execute(&mut ctx, "journal", &json!({}));
        assert!(o.ok, "{}", o.output);
        assert!(
            o.output.contains("3 records total, 3 match"),
            "{}",
            o.output
        );
        assert!(
            o.output.contains("j#1 2026-01-01 10:00:00 user_msg"),
            "{}",
            o.output
        );
        assert!(
            o.output.contains("j#3 2026-03-01 10:00:00 tool_result"),
            "{}",
            o.output
        );
        assert!(
            o.output.contains("bash ok=false code=cancelled"),
            "{}",
            o.output
        );

        // kind filter
        let o = execute(&mut ctx, "journal", &json!({"kind": "file_diff"}));
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("src/main.rs +3 -1"), "{}", o.output);
        assert!(!o.output.contains("j#1 "), "{}", o.output);

        // time window: a bare date as `to` covers the whole day
        let o = execute(
            &mut ctx,
            "journal",
            &json!({"from": "2026-02-01", "to": "2026-02-01"}),
        );
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("j#2"), "{}", o.output);
        assert!(!o.output.contains("j#1 "), "{}", o.output);
        assert!(!o.output.contains("j#3 "), "{}", o.output);

        // paging and tailing
        let o = execute(&mut ctx, "journal", &json!({"after": 1, "last": 1}));
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("j#3"), "{}", o.output);
        assert!(!o.output.contains("j#2"), "{}", o.output);

        // substring query over rendered lines
        let o = execute(&mut ctx, "journal", &json!({"query": "MAIN.RS"}));
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("j#2"), "{}", o.output);
        assert!(!o.output.contains("j#1 "), "{}", o.output);

        // malformed bounds are rejected, not ignored
        let o = execute(&mut ctx, "journal", &json!({"from": "not-a-date"}));
        assert!(!o.ok, "{}", o.output);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn journal_reads_across_sessions_sorted() {
        let (mut ctx, dir) = proj();
        write_journal(
            &dir,
            "shared",
            &[rec(
                1,
                "2026-02-01T10:00:00+00:00",
                "user_msg",
                r#","chars":7"#,
            )],
        );
        write_journal(
            &dir,
            "older",
            &[
                rec(
                    1,
                    "2026-01-01T10:00:00+00:00",
                    "plan",
                    r#","op":"start","id":"p1""#,
                ),
                rec(
                    2,
                    "2026-03-01T10:00:00+00:00",
                    "compaction",
                    r#","phase":"begin""#,
                ),
            ],
        );
        let o = execute(&mut ctx, "journal", &json!({"session": "all"}));
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("all sessions"), "{}", o.output);
        // chronological across files, not filesystem order
        let first = o.output.find("j#1 ").unwrap();
        let second = o.output.find("j#2 ").unwrap();
        assert!(first < second, "{}", o.output);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn journal_rejects_path_like_session_ids() {
        let (mut ctx, dir) = proj();
        let o = execute(&mut ctx, "journal", &json!({"session": "../evil"}));
        assert!(!o.ok, "{}", o.output);
        let o = execute(&mut ctx, "journal", &json!({"session": "a\\b"}));
        assert!(!o.ok, "{}", o.output);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn journal_assumptions_op_lists_only_open() {
        let (mut ctx, dir) = proj();
        let journal_dir = dir.join(".sqwai").join("journal");
        fs::create_dir_all(&journal_dir).unwrap();
        let mut journal = crate::agent::journal::Journal::open(&dir, "shared").unwrap();
        let seq = journal
            .append(
                "note",
                json!({"by": "model", "note": "assumption", "text": "the CI is green"}),
            )
            .unwrap();
        journal
            .append(
                "note",
                json!({"by": "model", "note": "lesson", "text": "closed it", "resolves": seq}),
            )
            .unwrap();

        let o = execute(&mut ctx, "journal", &json!({"op": "assumptions"}));
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("no open assumptions"), "{}", o.output);

        journal
            .append(
                "note",
                json!({"by": "model", "note": "assumption", "text": "the API is stable"}),
            )
            .unwrap();
        let o = execute(&mut ctx, "journal", &json!({"op": "assumptions"}));
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("the API is stable"), "{}", o.output);
        assert!(!o.output.contains("the CI is green"), "{}", o.output);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn plan_cancel_without_id_is_refused() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "criteria": ["note"],
                "goal": "single plan cancel test",
                "steps": [{"title": "step 1"}]
            }),
        );
        assert!(created.ok, "{}", created.output);

        // Cancel with missing id: no silent whole-plan kill, even with a
        // single active plan — a forgotten id must not destroy the plan.
        let refused = plan_op(&mut ctx, &json!({"op": "cancel"}));
        assert!(!refused.ok, "{}", refused.output);
        assert!(
            refused.output.contains("need_step_id"),
            "{}",
            refused.output
        );

        // The plan is untouched and still active.
        assert!(plan::open_active(&dir).unwrap().is_some());

        // Cancelling a real step id still works.
        let cancelled = plan_op(
            &mut ctx,
            &json!({"op": "cancel", "id": "1", "reason": "skip"}),
        );
        assert!(cancelled.ok, "{}", cancelled.output);
        fs::remove_dir_all(&dir).ok();
    }

    /// Audit C3: two sessions on one plan (cross-process `--force`)
    /// interleave load→append→store without arbitration; the later store
    /// from a stale load regressed the other session's content AND its
    /// cursor, and replay could not heal it (Start/Start collides with the
    /// single-active-step invariant and strands the stream in `stalled`).
    /// With arbitration the stale commit is REFUSED before it journals:
    /// A's work stays intact, B is told to re-read and retry.
    #[test]
    fn plan_stale_session_commit_is_refused_not_regressed() {
        let (mut ctx_a, dir) = proj();
        let created = plan_op(
            &mut ctx_a,
            &json!({
                "op": "create",
                "criteria": ["note"],
                "goal": "race",
                "steps": [{"title": "one"}, {"title": "two"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;

        // session B loads the plan before A's next store
        let mut plan_b = plan::open(&dir, &plan_id).unwrap();

        // A starts step 1: journal intent, then store
        assert!(plan_op(&mut ctx_a, &json!({"op": "start", "id": "1"})).ok);

        // B starts step 2 on its stale copy — the commit must be refused
        assert!(
            plan::apply(
                &mut plan_b,
                plan::Op::Start {
                    id: "2".into(),
                    confirm: None
                },
                &plan::Limits::default(),
                None,
            )
            .is_ok()
        );
        let err = plan::commit(
            &dir,
            "sess-b",
            &mut plan_b,
            "start",
            "model",
            true,
            json!({"id": "2"}),
        )
        .expect_err("a stale-base commit must be refused");
        assert!(err.to_string().contains("plan_moved"), "{err:#}");

        // A's work is intact, B's op left no trace on disk or in the journal
        let on_disk = plan::open(&dir, &plan_id).unwrap();
        assert_eq!(
            on_disk.step("1").unwrap().status,
            plan::StepStatus::InProgress,
            "A's start must survive"
        );
        assert_eq!(
            on_disk.step("2").unwrap().status,
            plan::StepStatus::Pending,
            "B's refused op must not half-land"
        );
        assert!(
            !dir.join(".sqwai/journal/sess-b.jsonl").exists(),
            "a refused commit must not journal an intent (it would stall replay)"
        );

        // nothing to heal, nothing stalled
        let report = plan::replay(&dir).unwrap();
        assert!(report.stalled.is_empty(), "{report:?}");

        // B's retry path: re-read, and the legitimate conflict is now
        // VISIBLE — step 1 is in progress, so starting step 2 is rejected
        // by the invariant instead of silently clobbering A
        let fresh = plan::open(&dir, &plan_id).unwrap();
        let mut fresh = fresh;
        let rejected = plan::apply(
            &mut fresh,
            plan::Op::Start {
                id: "2".into(),
                confirm: None,
            },
            &plan::Limits::default(),
            None,
        );
        assert!(rejected.is_err(), "single-active-step must reject B now");
        fs::remove_dir_all(&dir).ok();
    }

    /// Evidence journaled but lost from the file (crash between the journal
    /// write and the plan store) must still re-attach on replay even when a
    /// later finish already moved the cursor past it. Pre-fix: the evidence
    /// branch only saw records with `seq > cursor`, so the step finished
    /// with no evidence and the receipt was lost forever.
    #[test]
    fn plan_replay_reattaches_evidence_behind_the_cursor() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "criteria": ["note"],
                "goal": "evidence recovery",
                "steps": [{"title": "step 1"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        let started = plan_op(&mut ctx, &json!({"op": "start", "id": "1"}));
        assert!(started.ok, "{}", started.output);
        // tool evidence lands in the journal (live path attaches it too)
        let mut journal = crate::agent::journal::Journal::open(&dir, &ctx.session_id).unwrap();
        journal.set_attribution(Some("1".into()), Some(plan_id.clone()), "main");
        let evidence_seq = journal
            .append("file_diff", json!({"path": "src/x.rs", "ok": true}))
            .unwrap();
        // crash: journal has it, the plan file does not
        let mut plan = plan::open(&dir, &plan_id).unwrap();
        plan.step_mut("1").unwrap().evidence.clear();
        plan::store(&dir, &plan).unwrap();
        // a later finish moves the cursor past the evidence seq
        let finished = plan_op(
            &mut ctx,
            &json!({"op": "finish", "id": "1", "summary": "done"}),
        );
        assert!(finished.ok, "{}", finished.output);
        let report = plan::replay(&dir).unwrap();
        let healed = plan::open(&dir, &plan_id).unwrap();
        let step = healed.step("1").expect("step must survive");
        assert!(
            step.evidence
                .iter()
                .any(|reference| reference.seq == evidence_seq),
            "replay must re-attach evidence seq {evidence_seq} (report: {report:?})"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// A plan created after an abandonment whose file tears must rebuild from
    /// the journaled create intent — not quarantine, and without resurrecting
    /// the abandoned sibling.
    #[test]
    fn plan_corrupt_rebuild_keeps_abandoned_sibling_down() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "old plan",
                "criteria": ["old note"],
                "steps": [{"title": "old step"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let old_id = plan::open_active(&dir).unwrap().unwrap().id;
        let abandoned = plan_op(
            &mut ctx,
            &json!({"op": "abandon", "reason": "the goal moved to a different task"}),
        );
        assert!(abandoned.ok, "{}", abandoned.output);
        let replacement = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "new plan",
                "criteria": ["new note"],
                "steps": [{"title": "new step"}]
            }),
        );
        assert!(replacement.ok, "{}", replacement.output);
        let new_id = plan::open_active(&dir).unwrap().unwrap().id;
        assert_ne!(old_id, new_id, "the replacement is a different plan");
        // a later op on the new plan (it is the only active one)
        let started = plan_op(&mut ctx, &json!({"op": "start", "id": "1"}));
        assert!(started.ok, "{}", started.output);
        // torn write on the new plan's file, journal intact
        std::fs::write(
            plan::plans_dir(&dir).join(format!("{new_id}.json")),
            "{torn",
        )
        .unwrap();
        let rebuilt = plan::open(&dir, &new_id).expect("torn plan file must rebuild");
        assert_eq!(rebuilt.goal.text, "new plan");
        assert_eq!(
            rebuilt.step("1").unwrap().status,
            plan::StepStatus::InProgress,
            "later ops must survive the rebuild"
        );
        // the sibling stays abandoned — the rebuild must not resurrect it
        let sibling = plan::open(&dir, &old_id).unwrap();
        assert_eq!(sibling.status, plan::PlanStatus::Abandoned);
        fs::remove_dir_all(&dir).ok();
    }

    /// A user-blocked command inside `cmd:` acceptance never runs, even
    /// though the classifier alone would pass it: pre-fix the runners
    /// never consulted `[safety].blocked_patterns`, so `echo` (Safe)
    /// Finishing a step reports its blast radius in one line: the files
    /// the step wrote (journal file_diff chain, subagent sessions
    /// included). A step that wrote nothing stays silent.
    #[test]
    fn plan_finish_reports_blast_radius() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "blast radius",
                "acceptance": ["manual: eyeball it"],
                "steps": [{"title": "work"}, {"title": "look"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        // the step wrote two files (one from a subagent session)
        let mut journal = crate::agent::journal::Journal::open(&dir, &ctx.session_id).unwrap();
        journal.set_attribution(Some("1".into()), Some(plan_id.clone()), "main");
        journal
            .append("file_diff", json!({"path": "src/a.rs"}))
            .unwrap();
        let mut sub = crate::agent::journal::Journal::open(&dir, "sub-1").unwrap();
        sub.set_attribution(Some("1".into()), Some(plan_id.clone()), "main");
        sub.append("file_diff", json!({"path": "src/b.rs"}))
            .unwrap();
        let finished = plan_op(
            &mut ctx,
            &json!({"op": "finish", "id": "1", "summary": "done"}),
        );
        assert!(finished.ok, "{}", finished.output);
        assert!(
            finished
                .output
                .contains("blast radius: step 1 touched 2 file(s) (src/a.rs, src/b.rs)"),
            "{}",
            finished.output
        );
        // a step that wrote nothing reports nothing
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "2"})).ok);
        let finished = plan_op(
            &mut ctx,
            &json!({"op": "finish", "id": "2", "summary": "looked"}),
        );
        assert!(finished.ok, "{}", finished.output);
        assert!(
            !finished.output.contains("blast radius"),
            "{}",
            finished.output
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// Journal-first for step cancel: the dispatcher must commit (not bare
    /// store), or crash recovery never sees the cancellation. First asserts
    /// the commit record exists; then simulates the crash between commit
    /// and store (intent journaled, file untouched) and requires replay to
    /// land the cancellation instead of reopening the step.
    #[test]
    fn plan_cancel_is_journaled_for_crash_recovery() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "criteria": ["note"],
                "goal": "cancel recovery",
                "steps": [{"title": "step 1"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        let cancelled = plan_op(
            &mut ctx,
            &json!({"op": "cancel", "id": "1", "reason": "skip"}),
        );
        assert!(cancelled.ok, "{}", cancelled.output);
        // the commit record must exist (pre-fix: bare store wrote nothing)
        let records = crate::agent::journal::Journal::records_for(&dir, &ctx.session_id).unwrap();
        assert!(
            records.iter().any(|record| {
                record.kind == "plan"
                    && record.fields.get("op").and_then(|value| value.as_str()) == Some("cancel")
                    && record.fields.get("id").and_then(|value| value.as_str()) == Some("1")
            }),
            "cancel intent must be journaled"
        );
        // the cancel intent, journaled the way plan::commit journals it,
        // with no store after it (the crash)
        let mut journal = crate::agent::journal::Journal::open(&dir, &ctx.session_id).unwrap();
        journal
            .append(
                "plan",
                serde_json::json!({
                    "op": "cancel", "id": "1", "reason": "skip",
                    "plan_id": plan_id, "by": "model", "ok": true,
                }),
            )
            .unwrap();
        let report = plan::replay(&dir).unwrap();
        assert!(report.ops_applied >= 1, "{report:?}");
        let rebuilt = plan::open(&dir, &plan_id).unwrap();
        let step = rebuilt.step("1").expect("step must survive");
        assert_eq!(
            step.status,
            plan::StepStatus::Cancelled,
            "replay must restore the cancellation, not reopen the step"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn plan_cancel_with_plan_id_is_user_only() {
        let (mut ctx, dir) = proj();
        let p1 = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "criteria": ["note"],
                "goal": "first plan",
                "steps": [{"title": "step 1"}]
            }),
        );
        assert!(p1.ok);
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;

        // Even spelled out, whole-plan abandon is the user's call.
        let refused = plan_op(&mut ctx, &json!({"op": "cancel", "id": plan_id}));
        assert!(!refused.ok, "{}", refused.output);
        assert!(
            refused.output.contains("abandon_user_only"),
            "{}",
            refused.output
        );
        assert!(plan::open_active(&dir).unwrap().is_some());
        fs::remove_dir_all(&dir).ok();
    }

    /// The goal-ownership attack: cancel (no id) → create with a new goal
    /// and weaker constraints. Both halves must refuse; the original goal
    /// and constraints survive byte-for-byte.
    #[test]
    fn cancel_create_cannot_rewrite_the_goal() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "original goal",
                "constraints": ["keep the format"],
                "acceptance": ["manual: eyeball it"],
                "steps": [{"title": "step 1"}]
            }),
        );
        assert!(created.ok, "{}", created.output);

        // half 1: silent abandon refuses
        let cancelled = plan_op(&mut ctx, &json!({"op": "cancel"}));
        assert!(!cancelled.ok, "{}", cancelled.output);

        // half 2: spelled-out abandon refuses too
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        let abandoned = plan_op(&mut ctx, &json!({"op": "cancel", "id": plan_id}));
        assert!(!abandoned.ok, "{}", abandoned.output);

        // so the replacement create is refused: a plan is still active
        let replaced = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "weaker goal",
                "constraints": [],
                "acceptance": ["manual: eyeball it"],
                "steps": [{"title": "step 1"}]
            }),
        );
        assert!(!replaced.ok, "{}", replaced.output);
        assert!(
            replaced.output.contains("plan_exists"),
            "{}",
            replaced.output
        );

        // original goal and constraints untouched
        let after = plan::open_active(&dir).unwrap().unwrap();
        assert_eq!(after.goal.text, "original goal");
        assert_eq!(after.constraints, vec!["keep the format".to_string()]);
        assert_eq!(after.status, plan::PlanStatus::Active);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn step_diff_tool_flow() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "criteria": ["note"],
                "goal": "step diff test",
                "steps": [
                    {"title": "first step", "kind": "change"},
                    {"title": "second step", "kind": "change"}
                ]
            }),
        );
        assert!(created.ok);

        // 1. step_diff requires step_id
        let no_id = plan_op(&mut ctx, &json!({"op": "step_diff"}));
        assert!(!no_id.ok);
        assert!(no_id.output.contains("step_id"));

        // 2. Pending step reports it has not started
        let pending = plan_op(&mut ctx, &json!({"op": "step_diff", "step_id": "1"}));
        assert!(pending.ok);
        assert!(pending.output.contains("pending"));

        // 3. Start step 1, mutate a file
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        let write1 = execute(
            &mut ctx,
            "write",
            &json!({"file_path": "src/feature1.rs", "content": "pub fn feat1() {}\n"}),
        );
        assert!(write1.ok);

        // Diff while step 1 is in progress
        let diff_prog = plan_op(&mut ctx, &json!({"op": "step_diff", "step_id": "1"}));
        assert!(diff_prog.ok, "{}", diff_prog.output);
        assert!(diff_prog.output.contains("feat1"));
        assert!(diff_prog.output.contains("feature1.rs"));

        // Finish step 1
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        let mut journal = crate::agent::journal::Journal::open(&dir, &ctx.session_id).unwrap();
        journal.set_attribution(Some("1".into()), Some(plan_id.clone()), "main");
        journal
            .append("plan", json!({"op": "start", "id": "1"}))
            .unwrap();
        journal
            .append_evidence(
                "file_diff",
                json!({
                    "path": "src/feature1.rs",
                    "added": 1,
                    "removed": 0,
                    "hash_after": "abc",
                    "mode": "100644",
                }),
            )
            .unwrap();

        assert!(
            plan_op(
                &mut ctx,
                &json!({"op": "finish", "id": "1", "summary": "done 1"})
            )
            .ok
        );

        // 4. Start step 2, mutate another file
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "2"})).ok);
        let write2 = execute(
            &mut ctx,
            "write",
            &json!({"file_path": "src/feature2.rs", "content": "pub fn feat2() {}\n"}),
        );
        assert!(write2.ok);

        let mut journal2 = crate::agent::journal::Journal::open(&dir, &ctx.session_id).unwrap();
        journal2.set_attribution(Some("2".into()), Some(plan_id), "main");
        journal2
            .append("plan", json!({"op": "start", "id": "2"}))
            .unwrap();
        journal2
            .append_evidence(
                "file_diff",
                json!({
                    "path": "src/feature2.rs",
                    "added": 1,
                    "removed": 0,
                    "hash_after": "xyz",
                    "mode": "100644",
                }),
            )
            .unwrap();

        assert!(
            plan_op(
                &mut ctx,
                &json!({"op": "finish", "id": "2", "summary": "done 2"})
            )
            .ok
        );

        // 5. Querying step 1 diff shows only step 1 changes
        let diff1 = plan_op(&mut ctx, &json!({"op": "step_diff", "step_id": "1"}));
        assert!(diff1.ok, "{}", diff1.output);
        assert!(diff1.output.contains("feature1.rs"));
        assert!(diff1.output.contains("feat1"));
        assert!(!diff1.output.contains("feature2.rs"));

        // 6. Querying step 2 diff shows only step 2 changes
        let diff2 = plan_op(&mut ctx, &json!({"op": "step_diff", "step_id": "2"}));
        assert!(diff2.ok, "{}", diff2.output);
        assert!(diff2.output.contains("feature2.rs"));
        assert!(diff2.output.contains("feat2"));
        assert!(!diff2.output.contains("feat1"));

        // 7. Path-scoped step diff
        let diff_path = plan_op(
            &mut ctx,
            &json!({"op": "step_diff", "step_id": "2", "path": "src/feature2.rs"}),
        );
        assert!(diff_path.ok, "{}", diff_path.output);
        assert!(diff_path.output.contains("feature2.rs"));

        fs::remove_dir_all(&dir).ok();
    }
}
