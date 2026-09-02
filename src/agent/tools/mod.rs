#![allow(dead_code)]
//! Built-in tool registry (phase 2).
//!
//! Each tool declares its JSON schema for the model and a handler. Handlers
//! receive a [`ToolCtx`] carrying the project root and session-scoped guard
//! state (which files were read, checkpoint journal).

mod exec;
mod fs;

use crate::plan;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// whether a tool may run in parallel with others
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// pure: never mutates the worktree
    ReadOnly,
    /// mutates files or runs processes; runs alone, gets a checkpoint
    Mutating,
}

#[derive(Clone)]
pub struct ToolCtx {
    /// project root; every path must resolve inside it
    pub root: PathBuf,
    /// files successfully read this session (guards edit/write)
    pub files_read: HashSet<PathBuf>,
    /// journal of checkpoints created by this session's mutations
    pub journal: Vec<(String, String)>,
}

impl ToolCtx {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            files_read: HashSet::new(),
            journal: Vec::new(),
        }
    }

    /// resolve a user-supplied path inside the project; rejects escapes
    pub fn resolve(&self, p: &str) -> Result<PathBuf, String> {
        let joined = if Path::new(p).is_absolute() {
            PathBuf::from(p)
        } else {
            self.root.join(p)
        };
        // canonicalize the deepest existing ancestor to defeat `..` and symlinks
        let mut anc = joined.clone();
        while !anc.exists() {
            match anc.parent() {
                Some(par) => anc = par.to_path_buf(),
                None => break,
            }
        }
        let canon = anc
            .canonicalize()
            .map_err(|e| format!("cannot resolve path {}: {e}", joined.display()))?;
        let root_canon = self
            .root
            .canonicalize()
            .map_err(|e| format!("bad project root: {e}"))?;
        if !canon.starts_with(&root_canon) {
            return Err(format!(
                "path '{}' escapes the project directory",
                joined.display()
            ));
        }
        Ok(joined)
    }

    fn mark_read(&mut self, p: &Path) {
        self.files_read.insert(p.to_path_buf());
    }

    fn was_read(&self, p: &Path) -> bool {
        self.files_read.contains(p)
    }
}

pub struct Outcome {
    pub ok: bool,
    /// short result the model (and the collapsed TUI row) sees
    pub output: String,
    /// unified diff of a file mutation, shown in the TUI when expanded
    pub diff: Option<String>,
}

impl Outcome {
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            ok: true,
            output: output.into(),
            diff: None,
        }
    }
    pub fn err(output: impl Into<String>) -> Self {
        Self {
            ok: false,
            output: output.into(),
            diff: None,
        }
    }
    /// attach a unified diff, keeping the short summary
    pub fn with_diff(mut self, diff: String) -> Self {
        if !diff.is_empty() {
            self.diff = Some(diff);
        }
        self
    }
}

struct ToolDef {
    name: &'static str,
    kind: Kind,
    description: &'static str,
    parameters: Value,
}

fn defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "read",
            kind: Kind::ReadOnly,
            description: "Read a file from the project. Returns numbered lines. \
Must be called before edit/write on an existing file.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string", "description": "path relative to the project root"},
                    "offset": {"type": "integer", "description": "1-based first line to read"},
                    "limit": {"type": "integer", "description": "max lines to read"}
                },
                "required": ["file_path"]
            }),
        },
        ToolDef {
            name: "write",
            kind: Kind::Mutating,
            description: "Create a new file or completely overwrite an existing one. \
Overwriting an existing file requires reading it first.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["file_path", "content"]
            }),
        },
        ToolDef {
            name: "edit",
            kind: Kind::Mutating,
            description: "Replace exact text inside a file. old_string must appear exactly once \
unless replace_all is true. Requires reading the file first.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean"}
                },
                "required": ["file_path", "old_string", "new_string"]
            }),
        },
        ToolDef {
            name: "multi_edit",
            kind: Kind::Mutating,
            description: "Apply several exact replacements to one file atomically.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string"},
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": {"type": "string"},
                                "new_string": {"type": "string"},
                                "replace_all": {"type": "boolean"}
                            },
                            "required": ["old_string", "new_string"]
                        }
                    }
                },
                "required": ["file_path", "edits"]
            }),
        },
        ToolDef {
            name: "ls",
            kind: Kind::ReadOnly,
            description: "List one directory's entries (name, type, size).",
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}}
            }),
        },
        ToolDef {
            name: "glob",
            kind: Kind::ReadOnly,
            description: "Find files by glob pattern (respects .gitignore). Example: src/**/*.rs",
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "description": "base dir, default project root"}
                },
                "required": ["pattern"]
            }),
        },
        ToolDef {
            name: "grep",
            kind: Kind::ReadOnly,
            description: "Regex search over file contents. Returns file:line: text matches.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "description": "dir or file to search"},
                    "include": {"type": "string", "description": "filename glob filter, e.g. *.rs"}
                },
                "required": ["pattern"]
            }),
        },
        ToolDef {
            name: "bash",
            kind: Kind::Mutating,
            description: "Run a shell command in the project directory. Destructive or risky commands \
(rm -rf, sudo, disk ops, force-push, etc.) require user approval and the model should avoid them. \
Long output is truncated to a tail and the full log path is returned. Use background=true for \
long-running commands.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "the shell command line to run"},
                    "timeout": {"type": "integer", "description": "seconds; kills the process on expiry"},
                    "background": {"type": "boolean", "description": "detach and return immediately"}
                },
                "required": ["command"]
            }),
        },
        ToolDef {
            name: "plan_update",
            kind: Kind::Mutating,
            description: "Replace the hidden project plan. Keep it compact and structured with Task, Status, Steps, Decisions & Gotchas, Files touched, and Next immediate action.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string"},
                    "status": {"type": "string"},
                    "steps": {"type": "array", "items": {"type": "string"}},
                    "decisions": {"type": "array", "items": {"type": "string"}},
                    "files": {"type": "array", "items": {"type": "string"}},
                    "next_action": {"type": "string"},
                    "context_limit": {"type": "integer", "description": "model context limit in tokens"}
                },
                "required": ["task", "status", "steps", "next_action"]
            }),
        },
        ToolDef {
            name: "todowrite",
            kind: Kind::ReadOnly,
            description: "Record a visible, user-facing to-do list. Call with a full replacement \
list of checkboxes: ['- [ ] item', '- [x] done', ...]. Used to keep the user informed of multi-step work.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "complete replacement list of '- [ ] ...' / '- [x] ...' lines"
                    }
                },
                "required": ["todos"]
            }),
        },
        ToolDef {
            name: "ask_user",
            kind: Kind::ReadOnly,
            description: "Ask the user a structured question with 2-5 answer options (and optional \
multiple choice). Use only for decisions that materially change the outcome (approach, library, \
schema), never for trivial clarification. The user can also type a free-text answer.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string"},
                    "options": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "label": {"type": "string"},
                                "description": {"type": "string"}
                            },
                            "required": ["label"]
                        },
                        "minItems": 2,
                        "maxItems": 5
                    },
                    "multiple": {"type": "boolean", "description": "allow selecting several options"},
                    "allow_free": {"type": "boolean", "description": "allow a custom typed answer"}
                },
                "required": ["question", "options"]
            }),
        },
    ]
}

/// kind of a registered tool, or `None` when the name is unknown
pub fn kind_of(name: &str) -> Option<Kind> {
    defs().into_iter().find(|d| d.name == name).map(|d| d.kind)
}

/// true when the tool can change the worktree or run processes
pub fn is_mutating(name: &str) -> bool {
    matches!(kind_of(name), Some(Kind::Mutating))
}

/// one-line description of a call's arguments for the live TUI row
pub fn call_summary(name: &str, args: &Value) -> String {
    let s = |k: &str| args[k].as_str().unwrap_or_default().to_string();
    match name {
        "ls" => s("path"),
        "read" | "write" | "edit" | "multi_edit" => s("file_path"),
        "bash" => s("command"),
        "glob" | "grep" => s("pattern"),
        "todowrite" => format!(
            "{} items",
            args["todos"].as_array().map(|a| a.len()).unwrap_or(0)
        ),
        "ask_user" => s("question"),
        _ => String::new(),
    }
}

/// Schemas sent to the model.
///
/// Sorted by name, never by registration order: the tool block is part of the
/// request prefix, so it must be byte-identical between requests for a
/// prefix cache to hit.
///
/// `plan_mode` narrows the set to read-only tools plus `plan_update`, so a
/// request that cannot mutate anything also does not pay for the mutating
/// schemas.
pub fn tool_specs(plan_mode: bool) -> Vec<crate::providers::ToolSpec> {
    let mut specs: Vec<crate::providers::ToolSpec> = defs()
        .into_iter()
        .filter(|d| !plan_mode || d.kind == Kind::ReadOnly || d.name == "plan_update")
        .map(|d| crate::providers::ToolSpec {
            name: d.name.to_string(),
            description: d.description.to_string(),
            parameters: d.parameters,
        })
        .collect();
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    specs
}

const READ_MAX_BYTES: usize = 400_000;

/// dispatch one tool call
pub fn execute(ctx: &mut ToolCtx, name: &str, args: &Value) -> Outcome {
    match name {
        "read" => fs::read(ctx, args["file_path"].as_str().unwrap_or_default(), args),
        "write" => fs::write_file(
            ctx,
            args["file_path"].as_str().unwrap_or_default(),
            args["content"].as_str().unwrap_or_default(),
        ),
        "edit" => fs::edit(
            ctx,
            args["file_path"].as_str().unwrap_or_default(),
            args["old_string"].as_str().unwrap_or_default(),
            args["new_string"].as_str().unwrap_or_default(),
            args["replace_all"].as_bool().unwrap_or(false),
        ),
        "multi_edit" => {
            let edits: Vec<(String, String, bool)> = args["edits"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|e| {
                            (
                                e["old_string"].as_str().unwrap_or_default().to_string(),
                                e["new_string"].as_str().unwrap_or_default().to_string(),
                                e["replace_all"].as_bool().unwrap_or(false),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            fs::multi_edit(ctx, args["file_path"].as_str().unwrap_or_default(), &edits)
        }
        "ls" => fs::ls(ctx, args["path"].as_str().unwrap_or(".")),
        "glob" => fs::glob(
            ctx,
            args["pattern"].as_str().unwrap_or_default(),
            args["path"].as_str(),
        ),
        "grep" => fs::grep(
            ctx,
            args["pattern"].as_str().unwrap_or_default(),
            args["path"].as_str(),
            args["include"].as_str(),
        ),
        "bash" => exec::bash(
            ctx,
            args["command"].as_str().unwrap_or_default(),
            args["timeout"].as_u64(),
            args["background"].as_bool().unwrap_or(false),
        ),
        "plan_update" => {
            let content = plan::from_fields(args);
            match plan::update(
                &ctx.root,
                &content,
                args["context_limit"].as_u64().unwrap_or(32_000),
            ) {
                Ok(saved) => Outcome::ok(format!(
                    "plan updated ({} checklist items)",
                    plan::todo_items(&saved).len()
                )),
                Err(e) => Outcome::err(format!("plan update rejected: {e:#}")),
            }
        }
        // direct dispatch never answers "unknown tool"
        "todowrite" => Outcome::ok(format!(
            "to-do list updated ({} items)",
            args["todos"].as_array().map(|a| a.len()).unwrap_or(0)
        )),
        "ask_user" => Outcome::err("ask_user is served by the agent loop, not by the dispatcher"),
        other => Outcome::err(format!("unknown tool '{other}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

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
    fn read_then_edit_flow_and_guards() {
        let (mut ctx, dir) = proj();

        // edit before read is denied
        let o = execute(
            &mut ctx,
            "edit",
            &json!({"file_path": "src/main.rs", "old_string": "TODO", "new_string": "DONE"}),
        );
        assert!(!o.ok, "edit must require prior read");

        // read marks the file
        let o = execute(&mut ctx, "read", &json!({"file_path": "src/main.rs"}));
        assert!(o.ok && o.output.contains("TODO"), "{}", o.output);

        // now edit succeeds and content changes
        let o = execute(
            &mut ctx,
            "edit",
            &json!({"file_path": "src/main.rs", "old_string": "TODO", "new_string": "DONE"}),
        );
        assert!(o.ok, "{}", o.output);
        assert_eq!(
            fs::read_to_string(dir.join("src/main.rs")).unwrap(),
            "fn main() {}\n// DONE\n"
        );
        // checkpoint journal got an entry from the mutation
        assert_eq!(ctx.journal.len(), 1);
    }

    #[test]
    fn path_escape_is_rejected() {
        let (mut ctx, _dir) = proj();
        for p in ["../outside.txt", "..\\outside.txt", "C:\\Windows\\win.ini"] {
            let o = execute(&mut ctx, "read", &json!({"file_path": p}));
            assert!(!o.ok, "{p} must be rejected");
        }
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
    fn tool_specs_are_stably_sorted() {
        let names: Vec<String> = tool_specs(false).iter().map(|t| t.name.clone()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "tool order must not depend on registration");
        let again: Vec<String> = tool_specs(false).iter().map(|t| t.name.clone()).collect();
        assert_eq!(names, again, "the schema block must be byte-stable");
    }

    #[test]
    fn plan_mode_drops_mutating_schemas() {
        let names: Vec<String> = tool_specs(true).iter().map(|t| t.name.clone()).collect();
        assert!(names.contains(&"read".to_string()));
        assert!(names.contains(&"grep".to_string()));
        assert!(
            names.contains(&"plan_update".to_string()),
            "the plan has to stay writable in PLAN mode"
        );
        assert!(!names.contains(&"write".to_string()));
        assert!(!names.contains(&"edit".to_string()));
        assert!(!names.contains(&"bash".to_string()));
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
}
