#![allow(dead_code)]
//! Built-in tool registry (phase 2).
//!
//! Each tool declares its JSON schema for the model and a handler. Handlers
//! receive a [`ToolCtx`] carrying the project root and session-scoped guard
//! state (which files were read, checkpoint journal).

mod exec;
mod fs;
mod git;
pub(crate) mod web;

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
    /// secondary project instances can inspect but not mutate project state
    pub read_only: bool,
    /// files successfully read this session (guards edit/write)
    pub files_read: HashSet<PathBuf>,
    /// journal of checkpoints created by this session's mutations
    pub journal: Vec<(String, String)>,
}

impl ToolCtx {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_read_only(root, false)
    }

    pub fn with_read_only(root: impl Into<PathBuf>, read_only: bool) -> Self {
        Self {
            root: root.into(),
            read_only,
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
            name: "git_status",
            kind: Kind::ReadOnly,
            description: "Show the Git branch and worktree status.",
            parameters: json!({"type":"object","properties":{"porcelain":{"type":"boolean"}}}),
        },
        ToolDef {
            name: "git_diff",
            kind: Kind::ReadOnly,
            description: "Show unstaged Git diff, optionally limited to one path.",
            parameters: json!({"type":"object","properties":{"target":{"type":"string"}}}),
        },
        ToolDef {
            name: "git_log",
            kind: Kind::ReadOnly,
            description: "Show recent Git commits.",
            parameters: json!({"type":"object","properties":{"count":{"type":"integer","minimum":1,"maximum":100},"format":{"type":"string"}}}),
        },
        ToolDef {
            name: "git_commit",
            kind: Kind::Mutating,
            description: "Create a Git commit from currently staged changes, or all tracked changes when all is true.",
            parameters: json!({"type":"object","properties":{"message":{"type":"string"},"all":{"type":"boolean"}},"required":["message"]}),
        },
        ToolDef {
            name: "git_branch",
            kind: Kind::ReadOnly,
            description: "List branches or create/switch to a local branch.",
            parameters: json!({"type":"object","properties":{"action":{"type":"string","enum":["list","current","create","switch"]},"name":{"type":"string"}}}),
        },
        ToolDef {
            name: "patch",
            kind: Kind::Mutating,
            description: "Validate and apply a unified Git patch to the project.",
            parameters: json!({"type":"object","properties":{"patch":{"type":"string"}},"required":["patch"]}),
        },
        ToolDef {
            name: "websearch",
            kind: Kind::ReadOnly,
            description: "Search the web for a coding-related query and return a small set of normalized results.",
            parameters: json!({"type":"object","properties":{"query":{"type":"string"},"count":{"type":"integer","minimum":1,"maximum":10},"timeout":{"type":"integer","minimum":1,"maximum":60}},"required":["query"]}),
        },
        ToolDef {
            name: "webfetch",
            kind: Kind::ReadOnly,
            description: "Fetch a bounded HTTP(S) page or text response and return readable text. Use only user-provided or task-relevant URLs.",
            parameters: json!({"type":"object","properties":{"url":{"type":"string"},"timeout":{"type":"integer","minimum":1,"maximum":60}},"required":["url"]}),
        },
        ToolDef {
            name: "subagent",
            kind: Kind::ReadOnly,
            description: "Delegate one or more focused tasks to child agents. Children inherit the current Plan/Act mode; up to 8 tasks are accepted, at most 4 run concurrently, and child agents cannot create further subagents.",
            parameters: json!({"type":"object","properties":{"task":{"type":"string","description":"one focused child task"},"tasks":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":8,"description":"focused child tasks to run concurrently"}},"anyOf":[{"required":["task"]},{"required":["tasks"]}]}),
        },
        ToolDef {
            name: "plan",
            kind: Kind::Mutating,
            description: "Work the structured plan, one operation per call. Ops: create, start, \
finish, block, unblock, cancel, add, split, verify, complete, propose_goal_revision, show. Call \
show first if you are unsure of the current step ids. The host owns the goal, the constraints, \
acceptance status and evidence: you can only propose a goal revision, never apply one.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "op": {"type": "string", "enum": [
                        "create", "start", "finish", "block", "unblock", "cancel",
                        "add", "split", "verify", "complete", "propose_goal_revision", "show"
                    ]},
                    "id": {"type": "string", "description": "step id"},
                    "goal": {"type": "string", "description": "create / propose_goal_revision"},
                    "constraints": {"type": "array", "items": {"type": "string"}},
                    "acceptance": {
                        "type": ["array", "integer"],
                        "items": {"type": "string"},
                        "description": "create: criteria; verify: index"
                    },
                    "steps": {
                        "type": "array",
                        "description": "create: initial steps (3-12)",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": {"type": "string"},
                                "kind": {"type": "string", "enum": ["research", "change", "verify"]},
                                "refs": {"type": "array", "items": {"type": "string"}}
                            },
                            "required": ["title"]
                        }
                    },
                    "into": {
                        "type": "array",
                        "description": "split: the parts the step becomes",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": {"type": "string"},
                                "kind": {"type": "string", "enum": ["research", "change", "verify"]},
                                "refs": {"type": "array", "items": {"type": "string"}}
                            },
                            "required": ["title"]
                        }
                    },
                    "after": {"type": "string", "description": "add: insert after this step id"},
                    "title": {"type": "string", "description": "add: new step title"},
                    "kind": {"type": "string", "enum": ["research", "change", "verify"]},
                    "refs": {"type": "array", "items": {"type": "string"}},
                    "summary": {"type": "string", "description": "finish: what changed and where"},
                    "reason": {"type": "string", "description": "block / cancel / propose_goal_revision"},
                    "confirm": {"type": "boolean", "description": "start: re-read a stale step"},
                    "evidence": {"type": "array", "items": {"type": "integer"}},
                    "context_limit": {"type": "integer", "description": "model context in tokens"}
                },
                "required": ["op"]
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

/// Whether this particular call mutates state. `git_branch` contains both
/// read-only inspection and Act-only branch changes, so its action matters.
pub fn is_mutating_call(name: &str, args: &Value) -> bool {
    if name == "git_branch" {
        return matches!(args["action"].as_str(), Some("create" | "switch"));
    }
    is_mutating(name)
}

/// one-line description of a call's arguments for the live TUI row
pub fn call_summary(name: &str, args: &Value) -> String {
    let s = |k: &str| args[k].as_str().unwrap_or_default().to_string();
    match name {
        "ls" => s("path"),
        "read" | "write" | "edit" | "multi_edit" => s("file_path"),
        "bash" => s("command"),
        "glob" | "grep" => s("pattern"),
        "git_diff" => s("target"),
        "git_commit" => s("message"),
        "git_branch" => {
            let action = s("action");
            let name = s("name");
            format!("{action} {name}").trim().to_string()
        }
        "patch" => format!(
            "{} bytes",
            args["patch"].as_str().map(str::len).unwrap_or(0)
        ),
        "webfetch" => s("url"),
        "websearch" => s("query"),
        "subagent" => args["tasks"]
            .as_array()
            .map(|tasks| format!("{} tasks", tasks.len()))
            .unwrap_or_else(|| s("task")),
        "ask_user" => s("question"),
        "plan" => format!("plan {}", s("op")),
        _ => String::new(),
    }
}

/// Schemas sent to the model.
///
/// Sorted by name, never by registration order: the tool block is part of the
/// request prefix, so it must be byte-identical between requests for a
/// prefix cache to hit.
///
/// `plan_mode` narrows the set to read-only tools plus `plan`, so a request
/// that cannot mutate the project still lets the model build and refine the
/// plan (§5.3) without paying for the mutating schemas.
pub fn tool_specs(plan_mode: bool) -> Vec<crate::providers::ToolSpec> {
    let mut specs: Vec<crate::providers::ToolSpec> = defs()
        .into_iter()
        .filter(|d| !plan_mode || d.kind == Kind::ReadOnly || d.name == "plan")
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
    if ctx.read_only
        && matches!(
            name,
            "write"
                | "edit"
                | "multi_edit"
                | "git_commit"
                | "git_branch"
                |             "patch"
                | "bash"
                | "plan"
        )
    {
        return Outcome::err(
            "project is read-only because another sqwai instance owns the lock; use --force to enable writes",
        );
    }
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
        "git_status" => git::status(ctx, args),
        "git_diff" => git::diff(ctx, args),
        "git_log" => git::log(ctx, args),
        "git_commit" => git::commit(ctx, args),
        "git_branch" => git::branch(ctx, args),
        "patch" => git::patch(ctx, args),
        "webfetch" | "websearch" => Outcome::err("web tools must run through the async dispatcher"),
        "bash" => exec::bash(
            ctx,
            args["command"].as_str().unwrap_or_default(),
            args["timeout"].as_u64(),
            args["background"].as_bool().unwrap_or(false),
        ),
        "plan" => plan_op(ctx, args),
        // direct dispatch never answers "unknown tool"
        "ask_user" => Outcome::err("ask_user is served by the agent loop, not by the dispatcher"),
        other => Outcome::err(format!("unknown tool '{other}'")),
    }
}

/// The `plan` tool: one operation per call, validated by the host (§2.1.3).
fn plan_op(ctx: &mut ToolCtx, args: &Value) -> Outcome {
    let op: plan::Op = match serde_json::from_value(args.clone()) {
        Ok(op) => op,
        Err(e) => {
            return Outcome::err(format!(
                "plan op rejected: {e} — call plan show to see the current plan"
            ))
        }
    };
    let limits = plan::Limits::default();

    match op {
        plan::Op::Create {
            goal,
            constraints,
            acceptance,
            steps,
        } => match plan::open_active(&ctx.root) {
            Ok(Some(existing)) => rejection(plan::Rejection {
                code: "plan_exists",
                reason: format!("an active plan already exists: {}", existing.id),
                hint: "use /plan to continue, complete or abandon it first".to_string(),
            }),
            Ok(None) => {
                let budget_limit = (args["context_limit"].as_u64().unwrap_or(32_000) / 10).max(256);
                match plan::create(goal, constraints, acceptance, steps, budget_limit, &limits) {
                    Ok(created) => {
                        let id = created.id.clone();
                        let steps = created.steps.len();
                        match plan::store(&ctx.root, &created) {
                            Ok(()) => Outcome::ok(format!("plan {id} created with {steps} steps")),
                            Err(e) => Outcome::err(format!("plan write failed: {e:#}")),
                        }
                    }
                    Err(r) => rejection(r),
                }
            }
            Err(e) => Outcome::err(format!("plan store unreadable: {e:#}")),
        },
        other => {
            let mut active = match plan::open_active(&ctx.root) {
                Ok(Some(p)) => p,
                Ok(None) => {
                    return Outcome::err("no active plan: create one with op=create first".to_string())
                }
                Err(e) => return Outcome::err(format!("plan store unreadable: {e:#}")),
            };
            match plan::apply(&mut active, other, &limits) {
                Ok(applied) => {
                    if let Err(e) = plan::store(&ctx.root, &active) {
                        return Outcome::err(format!("plan write failed: {e:#}"));
                    }
                    match applied {
                        plan::Applied::Created(_) => Outcome::ok("plan created".to_string()),
                        plan::Applied::Updated { message } => Outcome::ok(message),
                        plan::Applied::Proposed { goal, reason } => Outcome::ok(format!(
                            "goal revision proposed for the user to confirm: \"{goal}\" ({reason})"
                        )),
                        plan::Applied::Shown { text } => Outcome::ok(text),
                        plan::Applied::Completed => {
                            Outcome::ok(format!("plan {} completed", active.id))
                        }
                    }
                }
                Err(r) => {
                    // the rejection counter is plan state, so persist it too
                    let _ = plan::store(&ctx.root, &active);
                    rejection(r)
                }
            }
        }
    }
}

/// Rejections are a normal tool result the model can act on (§2.1.4).
fn rejection(r: plan::Rejection) -> Outcome {
    Outcome::err(
        json!({
            "ok": false,
            "code": r.code,
            "reason": r.reason,
            "hint": r.hint,
        })
        .to_string(),
    )
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
            names.contains(&"plan".to_string()),
            "the plan has to stay writable in PLAN mode"
        );
        assert!(!names.contains(&"write".to_string()));
        assert!(!names.contains(&"edit".to_string()));
        assert!(!names.contains(&"bash".to_string()));
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

        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        let finish = plan_op(
            &mut ctx,
            &json!({"op": "finish", "id": "1", "summary": "schema added"}),
        );
        assert!(finish.ok, "{}", finish.output);

        let shown = plan_op(&mut ctx, &json!({"op": "show"}));
        assert!(shown.ok, "{}", shown.output);
        assert!(
            shown.output.contains("goal: wire the plan tool"),
            "{}",
            shown.output
        );
        assert!(
            shown.output.contains("[x] 1"),
            "step 1 should read as done:\n{}",
            shown.output
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn plan_rejections_carry_a_code_and_a_hint() {
        let (mut ctx, dir) = proj();
        plan_op(
            &mut ctx,
            &json!({"op": "create", "goal": "g", "steps": [{"title": "one"}]}),
        );
        // finishing a step that was never started
        let bad = plan_op(&mut ctx, &json!({"op": "finish", "id": "1", "summary": "x"}));
        assert!(!bad.ok);
        assert!(bad.output.contains("step_not_in_progress"), "{}", bad.output);
        assert!(bad.output.contains("hint"), "{}", bad.output);
        // a second create is refused while one is active
        let second = plan_op(
            &mut ctx,
            &json!({"op": "create", "goal": "h", "steps": [{"title": "two"}]}),
        );
        assert!(!second.ok);
        assert!(second.output.contains("plan_exists"), "{}", second.output);
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
}
