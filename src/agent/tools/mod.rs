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

use crate::agent::safety;
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
    /// canonicalized [`Self::root`], computed once at construction so the
    /// jail check in [`Self::resolve`] does not canonicalize the root on
    /// every call. Falls back to [`Self::root`] when canonicalization fails;
    /// every tool op fails downstream in that case anyway.
    root_canon: PathBuf,
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
        let root = root.into();
        // Canonicalize once: `resolve` compares canonical paths, and the root
        // itself may sit behind a symlink (macOS `/var` -> `/private/var`).
        let root_canon = root.canonicalize().unwrap_or_else(|_| root.clone());
        Self {
            root,
            root_canon,
            read_only,
            files_read: HashSet::new(),
            journal: Vec::new(),
        }
    }

    /// resolve a user-supplied path inside the project; rejects escapes and
    /// host-owned state under `.sqwai/` (§2.0)
    pub fn resolve(&self, p: &str) -> Result<PathBuf, String> {
        let joined = if Path::new(p).is_absolute() {
            PathBuf::from(p)
        } else {
            self.root.join(p)
        };
        // canonicalize the deepest existing ancestor to defeat `..` and
        // symlinks, then re-append the part that does not exist yet, so the
        // full target is known even when the file is about to be created
        let mut anc = joined.clone();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        while !anc.exists() {
            let Some(name) = anc.file_name().map(|n| n.to_os_string()) else {
                break;
            };
            match anc.parent() {
                Some(par) => {
                    tail.push(name);
                    anc = par.to_path_buf();
                }
                None => break,
            }
        }
        let canon = anc
            .canonicalize()
            .map_err(|e| format!("cannot resolve path {}: {e}", joined.display()))?;
        let mut resolved = canon;
        for name in tail.iter().rev() {
            resolved.push(name);
        }
        let Ok(relative) = resolved.strip_prefix(&self.root_canon) else {
            return Err(format!(
                "path '{}' escapes the project directory",
                joined.display()
            ));
        };
        // Plan, journal, memory and graph are reachable only through their own
        // tools. Enforcing it here is what makes "host-written" and
        // "append-only" guarantees rather than requests (§2.0).
        if let Some(denied) = host_owned_denial(relative) {
            return Err(denied);
        }
        // Return `joined`, not `resolved`: the whole display layer
        // (`rel_label`, checkpoint labels, `FileDiff.path`) strips the
        // *uncanonicalized* root, and where the root itself is reached
        // through a symlink (macOS `/var` -> `/private/var`) a canonical
        // path would miss that prefix and leak absolute machine-specific
        // paths into the journal. The security decision above was already
        // made in canonical space, so the spelling returned here only
        // affects labels, never the verdict.
        Ok(joined)
    }

    fn mark_read(&mut self, p: &Path) {
        self.files_read.insert(p.to_path_buf());
    }

    fn was_read(&self, p: &Path) -> bool {
        self.files_read.contains(p)
    }
}

/// The agent's own state directory. File tools see only `skills/` and
/// `config.toml` inside it; plan, journal, memory and graph are host-owned and
/// reachable only through their dedicated tools (§2.0).
const STATE_DIR: &str = ".sqwai";

/// Deny message when `relative` points at host-owned state, `None` otherwise.
/// `relative` must already be relative to the canonicalized project root, so a
/// symlink pointing into `.sqwai/` cannot slip past this check.
fn host_owned_denial(relative: &Path) -> Option<String> {
    use std::path::Component;
    let mut components = relative.components();
    match components.next() {
        Some(Component::Normal(first)) if first == STATE_DIR => {}
        _ => return None,
    }
    let allowed = match components.next() {
        Some(Component::Normal(second)) => second == "skills" || second == "config.toml",
        // `.sqwai` itself: listing or writing the directory is not allowed
        _ => false,
    };
    if allowed {
        return None;
    }
    Some(format!(
        "path '{}' is host-owned state: plan, journal, memory and graph are reachable \
         only through their own tools (plan, note, memory_read, memory_propose). \
         Inside {STATE_DIR}/ the file tools may use skills/ and config.toml only.",
        relative.display()
    ))
}

pub struct FileDiff {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    pub hash_before: Option<String>,
    pub hash_after: String,
    pub mode: String,
    pub checkpoint: Option<String>,
}

pub struct Outcome {
    pub ok: bool,
    /// short result the model (and the collapsed TUI row) sees
    pub output: String,
    /// unified diff of a file mutation, shown in the TUI when expanded
    pub diff: Option<String>,
    /// host-derived metadata for the journal
    pub file_diff: Option<FileDiff>,
}

impl Outcome {
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            ok: true,
            output: output.into(),
            diff: None,
            file_diff: None,
        }
    }
    pub fn err(output: impl Into<String>) -> Self {
        Self {
            ok: false,
            output: output.into(),
            diff: None,
            file_diff: None,
        }
    }
    /// attach a unified diff, keeping the short summary
    pub fn with_diff(mut self, diff: String) -> Self {
        if !diff.is_empty() {
            self.diff = Some(diff);
        }
        self
    }

    pub fn with_file_diff(mut self, file_diff: FileDiff) -> Self {
        self.file_diff = Some(file_diff);
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
            name: "note",
            kind: Kind::ReadOnly,
            description: "Record a concise model note in the host journal.",
            parameters: json!({"type":"object","properties":{"note":{"type":"string"},"kind":{"type":"string","enum":["decision","rejected","assumption","lesson","blocker"]}},"required":["note","kind"]}),
        },
        ToolDef {
            name: "memory_read",
            kind: Kind::ReadOnly,
            description: "Read one host-owned daily diary entry. Date must use YYYY-MM-DD.",
            parameters: json!({"type":"object","properties":{"date":{"type":"string","description":"local diary date, YYYY-MM-DD"}},"required":["date"]}),
        },
        ToolDef {
            name: "memory_propose",
            kind: Kind::ReadOnly,
            description: "Propose a durable memory fact. The user must approve it before the host writes MEMORY.md or USER.md.",
            parameters: json!({"type":"object","properties":{"section":{"type":"string","enum":["Project","Conventions","User","Agreements"]},"scope":{"type":"string","enum":["project","user"]},"text":{"type":"string"},"replaces":{"type":"string"}},"required":["section","text"]}),
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
                    "evidence": {"type": "array", "items": {"type": "integer"}, "description": "deprecated informational field; host ignores it"},
                    "context_limit": {"type": "integer", "description": "model context in tokens"}
                },
                "required": ["op"]
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
        "memory_read" => s("date"),
        "memory_propose" => format!("{}: {}", s("scope"), s("text")),
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
pub fn tool_names() -> Vec<String> {
    let mut names: Vec<String> = defs().into_iter().map(|d| d.name.to_string()).collect();
    names.sort();
    names
}

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
                | "patch"
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
        "memory_read" => match crate::agent::diary::read_day(
            &ctx.root,
            args["date"].as_str().unwrap_or_default(),
        ) {
            Ok(text) => Outcome::ok(text),
            Err(message) => Outcome::err(message),
        },
        "memory_propose" => {
            let text = args["text"].as_str().unwrap_or_default().trim();
            let section = args["section"].as_str().unwrap_or("Project");
            let scope = args["scope"].as_str().unwrap_or("project");
            match crate::agent::memory::Scope::parse(scope) {
                Ok(scope) if !text.is_empty() => Outcome::ok(
                    serde_json::json!({
                        "proposal": "memory_propose",
                        "scope": scope.label(),
                        "section": section,
                        "text": crate::agent::diary::screen(text).text,
                        "replaces": args["replaces"].as_str(),
                    })
                    .to_string(),
                ),
                Ok(_) => Outcome::err("memory proposal text must not be empty"),
                Err(error) => Outcome::err(error),
            }
        }
        "note" => {
            let note = args["note"].as_str().unwrap_or_default().trim();
            let kind = args["kind"].as_str().unwrap_or_default().trim();
            if note.is_empty() || note.len() > 2000 {
                Outcome::err("note must be 1-2000 bytes")
            } else if !matches!(
                kind,
                "decision" | "rejected" | "assumption" | "lesson" | "blocker"
            ) {
                Outcome::err("note kind is invalid")
            } else {
                Outcome::ok(format!("note recorded: {kind}"))
            }
        }
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
            ));
        }
    };
    let limits = plan::Limits::default();

    let gate = if matches!(op, plan::Op::Complete) {
        validate_complete(ctx)
    } else {
        validate_evidence(&ctx.root, &op)
    };
    if let Err(message) = gate {
        return Outcome::err(message);
    }

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
        plan::Op::Verify {
            acceptance,
            evidence,
        } => verify_acceptance(ctx, acceptance, !evidence.is_empty()),
        other => {
            let mut active = match plan::open_active(&ctx.root) {
                Ok(Some(p)) => p,
                Ok(None) => {
                    return Outcome::err(
                        "no active plan: create one with op=create first".to_string(),
                    );
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

/// Longer than the tool default: an acceptance command is usually a test or
/// lint run, and cutting one off at two minutes would report a failure that is
/// really a timeout.
const ACCEPTANCE_TIMEOUT_SECS: u64 = 900;

/// `plan verify <index>` — the host settles the item, on its own terms.
///
/// A `cmd:` item is run here and now (§2.1.2: "host runs it on `plan verify`
/// and on `complete`"). A `Text` item needs host-recorded evidence that no
/// other acceptance item has already spent. A `manual:` item is refused: only
/// the user waives those.
fn verify_acceptance(ctx: &mut ToolCtx, index: usize, supplied: bool) -> Outcome {
    let mut active = match plan::open_active(&ctx.root) {
        Ok(Some(plan)) => plan,
        Ok(None) => return Outcome::err("no active plan: create one with op=create first"),
        Err(e) => return Outcome::err(format!("plan store unreadable: {e:#}")),
    };
    let Some(item) = active.acceptance.get(index) else {
        return rejection(plan::Rejection {
            code: "unknown_acceptance",
            reason: format!("no acceptance item {index}"),
            hint: "call plan show to see the acceptance list".to_string(),
        });
    };

    let evidence = match item.kind() {
        plan::AcceptanceKind::Manual(_) => Vec::new(),
        plan::AcceptanceKind::Command(command) => {
            let command = command.to_string();
            // The acceptance text arrives from the model on `plan create`, so
            // it is model-controlled input that the host is about to execute.
            // It goes through the same classifier as `bash`, and anything that
            // would need approval is refused rather than silently run: an
            // acceptance criterion is not the place to ask.
            if let safety::Verdict::NeedsApproval(reason) = safety::classify(&command) {
                return rejection(plan::Rejection {
                    code: "unsafe_acceptance",
                    reason: format!("acceptance {index} would run a {reason} command: {command}"),
                    hint: "acceptance commands run without asking, so they must be safe;                            rewrite it or have the user waive the item"
                        .to_string(),
                });
            }
            let run = exec::bash(ctx, &command, Some(ACCEPTANCE_TIMEOUT_SECS), false);
            if !run.ok {
                return rejection(plan::Rejection {
                    code: "acceptance_failed",
                    reason: format!("acceptance {index} command failed: {command}"),
                    hint: format!(
                        "fix what it reports, then verify again — {}",
                        run.output.lines().take(6).collect::<Vec<_>>().join(" / ")
                    ),
                });
            }
            Vec::new()
        }
        plan::AcceptanceKind::Text(_) => {
            let Some((step_id, evidence)) = unspent_verify_evidence(&active, index) else {
                return rejection(plan::Rejection {
                    code: "no_evidence",
                    reason: format!("acceptance {index} has no host evidence of its own"),
                    hint: "close a verify step whose evidence is not already spent on \
                           another acceptance item, or prefix the item with cmd: so the \
                           host can run it"
                        .to_string(),
                });
            };
            // The evidence still has to be what a verify step needs: a
            // successful exec or clean diagnostics. Removing the plan-wide
            // gate must not remove that.
            if let Err(message) = validate_attached_records(
                &ctx.root,
                &active.id,
                &step_id,
                plan::StepKind::Verify,
                &evidence,
            ) {
                return Outcome::err(message);
            }
            evidence
        }
    };

    match plan::verify_acceptance(&mut active, index, evidence, supplied) {
        Ok(applied) => {
            if let Err(e) = plan::store(&ctx.root, &active) {
                return Outcome::err(format!("plan write failed: {e:#}"));
            }
            match applied {
                plan::Applied::Updated { message } => Outcome::ok(message),
                _ => Outcome::ok(format!("acceptance {index} verified")),
            }
        }
        Err(r) => {
            let _ = plan::store(&ctx.root, &active);
            rejection(r)
        }
    }
}

/// A verify step whose evidence no acceptance item has spent yet, with that
/// evidence. `None` when every verify step's records are already accounted
/// for — which is the case this whole function exists to catch.
fn unspent_verify_evidence(
    active: &plan::Plan,
    index: usize,
) -> Option<(String, Vec<plan::EvidenceRef>)> {
    let spent: Vec<&plan::EvidenceRef> = active
        .acceptance
        .iter()
        .enumerate()
        .filter(|(other, item)| *other != index && item.status == plan::AcceptanceStatus::Verified)
        .flat_map(|(_, item)| item.evidence.iter())
        .collect();
    active
        .steps
        .iter()
        .filter(|step| step.kind == plan::StepKind::Verify)
        .find_map(|step| {
            let fresh: Vec<plan::EvidenceRef> = step
                .evidence
                .iter()
                .filter(|reference| {
                    !spent
                        .iter()
                        .any(|used| used.session == reference.session && used.seq == reference.seq)
                })
                .cloned()
                .collect();
            (!fresh.is_empty()).then(|| (step.id.clone(), fresh))
        })
}

/// The gate on `plan finish`: the step must have host-recorded evidence of the
/// right kind since it started.
///
/// `verify` is not handled here — `verify_acceptance` settles an acceptance
/// item on its own terms, per item, and this function used to short-circuit
/// that with a plan-wide "is there any verify evidence anywhere" check.
fn validate_evidence(root: &Path, op: &plan::Op) -> Result<(), String> {
    let plan::Op::Finish { id, .. } = op else {
        return Ok(());
    };
    let status = plan::open_active(root)
        .ok()
        .flatten()
        .and_then(|p| p.step(id).map(|s| s.status));
    if status != Some(plan::StepStatus::InProgress) {
        // `finish` on a step that is not in progress is rejected by the
        // validator with a clearer reason than a missing-evidence error.
        return Ok(());
    }
    let active = plan::open_active(root)
        .map_err(|e| format!("evidence_unreadable: {e:#}"))?
        .ok_or_else(|| "invalid_evidence: no active plan".to_string())?;
    let required_kind = active
        .step(id)
        .map(|step| step.kind)
        .ok_or_else(|| format!("unknown_step: no step {id}"))?;
    let evidence = active
        .step(id)
        .map(|step| step.evidence.clone())
        .unwrap_or_default();
    validate_attached_records(root, &active.id, id, required_kind, &evidence)
}

/// The gate on `plan complete`.
///
/// Every done step is re-checked against the journal, and every acceptance
/// item is settled again on its own terms: a `cmd:` item is **re-run** rather
/// than trusted from an earlier verify (§2.1.2 has the host run it "on `plan
/// verify` and on `complete`"; §2.4.11 makes the same point for the full test
/// suite), and a `Text` item is re-checked against the records it was verified
/// with. A waived item is the user's call and is left alone.
fn validate_complete(ctx: &mut ToolCtx) -> Result<(), String> {
    let root = ctx.root.clone();
    let active = plan::open_active(&root)
        .map_err(|e| format!("evidence_unreadable: {e:#}"))?
        .ok_or_else(|| "invalid_evidence: no active plan".to_string())?;
    for step in active
        .steps
        .iter()
        .filter(|step| step.status == plan::StepStatus::Done)
    {
        validate_attached_records(&root, &active.id, &step.id, step.kind, &step.evidence)?;
    }
    for (index, acceptance) in active.acceptance.iter().enumerate() {
        if acceptance.status != plan::AcceptanceStatus::Verified {
            continue;
        }
        match acceptance.kind() {
            plan::AcceptanceKind::Command(command) => {
                if let safety::Verdict::NeedsApproval(reason) = safety::classify(command) {
                    return Err(format!(
                        "unsafe_acceptance: acceptance {index} would run a {reason} command                          at completion: {command}"
                    ));
                }
                let run = exec::bash(ctx, command, Some(ACCEPTANCE_TIMEOUT_SECS), false);
                if !run.ok {
                    return Err(format!(
                        "acceptance_failed: acceptance {index} no longer passes: {command} — {}",
                        run.output.lines().take(6).collect::<Vec<_>>().join(" / ")
                    ));
                }
            }
            plan::AcceptanceKind::Manual(text) => {
                // Verified rather than waived: it should not have been possible
                // to get here, so say so instead of letting it slide.
                return Err(format!(
                    "invalid_evidence: acceptance {index} is manual ({text}) and can only be                      waived by the user"
                ));
            }
            plan::AcceptanceKind::Text(_) => {
                validate_attached_records(
                    &root,
                    &active.id,
                    "acceptance",
                    plan::StepKind::Verify,
                    &acceptance.evidence,
                )
                .map_err(|message| format!("acceptance {index}: {message}"))?;
            }
        }
    }
    Ok(())
}

fn validate_attached_records(
    root: &Path,
    plan_id: &str,
    step_id: &str,
    required_kind: plan::StepKind,
    evidence: &[plan::EvidenceRef],
) -> Result<(), String> {
    if evidence.is_empty() {
        return Err(format!(
            "no_evidence: step {step_id} requires journal evidence"
        ));
    }
    let after_seq = if step_id == "acceptance" {
        None
    } else {
        crate::agent::journal::Journal::step_started_at(root, plan_id, step_id)
            .map_err(|e| format!("evidence_unreadable: {e:#}"))?
    };
    let mut valid = 0usize;
    for reference in evidence {
        let record = crate::agent::journal::Journal::evidence(
            root,
            plan_id,
            if step_id == "acceptance" {
                None
            } else {
                Some(step_id)
            },
            reference,
            after_seq,
        )
        .map_err(|e| format!("evidence_unreadable: {e:#}"))?
        .ok_or_else(|| {
            format!(
                "invalid_evidence: journal record {} is not valid for this plan",
                reference.seq
            )
        })?;
        let allowed = match required_kind {
            plan::StepKind::Research => record.kind == "tool_result",
            plan::StepKind::Change => record.kind == "file_diff",
            plan::StepKind::Verify => {
                record.kind == "diagnostics"
                    || (record.kind == "tool_result"
                        && record.fields.get("ok").and_then(Value::as_bool) == Some(true)
                        && record
                            .fields
                            .get("tool")
                            .and_then(Value::as_str)
                            .is_some_and(|tool| tool == "bash" || tool.starts_with("git_")))
            }
        };
        if allowed {
            valid += 1;
        }
    }
    if valid == 0 {
        return Err(format!(
            "wrong_evidence: evidence does not satisfy {} step requirements",
            required_kind.as_str()
        ));
    }
    Ok(())
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
                    "acceptance":[],"steps":[{"title":"first"}]}),
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
            ".sqwai/graph/graph.db".to_string(),
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
        assert_eq!(names, tool_names());

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

        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        let mut evidence_journal =
            crate::agent::journal::Journal::open(&dir, "test-evidence").unwrap();
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

    #[test]
    fn evidence_must_match_step_kind_and_start_boundary() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "validate evidence",
                "steps": [
                    {"title": "research", "kind": "research"},
                    {"title": "change", "kind": "change"}
                ]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        let mut journal = crate::agent::journal::Journal::open(&dir, "evidence-rules").unwrap();

        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        journal.set_attribution(Some("1".into()), Some(plan_id.clone()), "main");
        journal.append("plan", json!({"op": "start"})).unwrap();
        journal
            .append_evidence("tool_result", json!({"tool": "read", "ok": true}))
            .unwrap();
        let research = plan_op(
            &mut ctx,
            &json!({"op": "finish", "id": "1", "summary": "researched", "evidence": [2]}),
        );
        assert!(research.ok, "{}", research.output);

        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "2"})).ok);
        journal.set_attribution(Some("2".into()), Some(plan_id.clone()), "main");
        journal.append("plan", json!({"op": "start"})).unwrap();
        let wrong_type = journal
            .append_evidence("tool_result", json!({"tool": "read", "ok": true}))
            .unwrap();
        let rejected = plan_op(
            &mut ctx,
            &json!({"op": "finish", "id": "2", "summary": "changed", "evidence": [wrong_type]}),
        );
        assert!(!rejected.ok);
        assert!(
            rejected.output.contains("wrong_evidence"),
            "{}",
            rejected.output
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// A `cmd:` acceptance item is settled by the host running the command,
    /// not by pointing at a journal record. This test used to pass a fabricated
    /// `bash` result as evidence for `cmd: cargo test` and see the item
    /// verified — the suite never ran.
    #[test]
    fn cmd_acceptance_is_verified_by_running_the_command() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "verify acceptance",
                "acceptance": ["cmd: exit 3", "cmd: exit 0"],
                "steps": [{"title": "verify", "kind": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);

        let failed = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!failed.ok, "{}", failed.output);
        assert!(
            failed.output.contains("acceptance_failed"),
            "a failing command must not verify: {}",
            failed.output
        );
        assert_eq!(
            plan::open_active(&dir).unwrap().unwrap().acceptance[0].status,
            plan::AcceptanceStatus::Pending
        );

        let passed = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 1}));
        assert!(passed.ok, "{}", passed.output);
        assert_eq!(
            plan::open_active(&dir).unwrap().unwrap().acceptance[1].status,
            plan::AcceptanceStatus::Verified
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// The acceptance text comes from the model on `plan create`, so it is
    /// model-controlled input the host is about to execute. It goes through the
    /// same classifier as `bash`, and anything that would need approval is
    /// refused rather than run without asking.
    #[test]
    fn cmd_acceptance_refuses_a_command_that_would_need_approval() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "sneak a command in",
                "acceptance": ["cmd: rm -rf /"],
                "steps": [{"title": "verify", "kind": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);

        let out = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!out.ok, "{}", out.output);
        assert!(out.output.contains("unsafe_acceptance"), "{}", out.output);
        fs::remove_dir_all(&dir).ok();
    }

    /// One host record cannot settle two acceptance items. Before this, verify
    /// took `steps.iter().find(kind == Verify && !evidence.is_empty())` — the
    /// first verify step with anything attached — so a single successful
    /// command let every item pass in turn on the same record.
    #[test]
    fn text_acceptance_cannot_reuse_another_items_evidence() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "two criteria, one check",
                "acceptance": ["the suite is green", "the linter is clean"],
                "steps": [{"title": "verify", "kind": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        let mut journal = crate::agent::journal::Journal::open(&dir, "verify-rules").unwrap();
        journal.set_attribution(Some("1".into()), Some(plan_id), "main");
        journal
            .append_evidence("tool_result", json!({"tool": "bash", "ok": true}))
            .unwrap();

        let first = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(first.ok, "{}", first.output);

        let second = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 1}));
        assert!(!second.ok, "the same record must not verify both");
        assert!(
            second.output.contains("no_evidence") || second.output.contains("evidence_spent"),
            "{}",
            second.output
        );
        assert_eq!(
            plan::open_active(&dir).unwrap().unwrap().acceptance[1].status,
            plan::AcceptanceStatus::Pending
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// A failed exec is not evidence of anything passing.
    #[test]
    fn text_acceptance_rejects_a_failed_exec_record() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "verify evidence",
                "acceptance": ["the suite is green"],
                "steps": [{"title": "verify", "kind": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        let mut journal = crate::agent::journal::Journal::open(&dir, "verify-rules").unwrap();
        journal.set_attribution(Some("1".into()), Some(plan_id), "main");
        journal
            .append_evidence("tool_result", json!({"tool": "bash", "ok": false}))
            .unwrap();

        let rejected = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!rejected.ok);
        assert!(
            rejected.output.contains("wrong_evidence"),
            "{}",
            rejected.output
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// `manual:` items are the user's call. Verify has to refuse them rather
    /// than quietly accept whatever evidence is lying around.
    #[test]
    fn manual_acceptance_is_refused_by_verify() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "manual check",
                "acceptance": ["manual: the panel looks right"],
                "steps": [{"title": "verify", "kind": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);

        let out = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!out.ok, "{}", out.output);
        assert!(out.output.contains("manual_acceptance"), "{}", out.output);
        fs::remove_dir_all(&dir).ok();
    }

    /// `complete` runs `cmd:` items again instead of trusting the verify that
    /// happened earlier: a criterion that stopped passing must block
    /// completion (§2.1.2).
    #[test]
    fn complete_reruns_cmd_acceptance_and_refuses_when_it_now_fails() {
        let (mut ctx, dir) = proj();
        let flag = dir.join("gate.txt");
        fs::write(&flag, "ok").unwrap();
        let command = format!("test -f {}", flag.display());
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "completion re-checks",
                "acceptance": [format!("cmd: {command}")],
                "steps": [{"title": "verify", "kind": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);

        let verified = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(verified.ok, "{}", verified.output);

        // the world changed after the verify
        fs::remove_file(&flag).unwrap();
        assert!(
            plan_op(
                &mut ctx,
                &json!({"op": "cancel", "id": "1", "reason": "done here"})
            )
            .ok
        );

        let completed = plan_op(&mut ctx, &json!({"op": "complete"}));
        assert!(!completed.ok, "{}", completed.output);
        assert!(
            completed.output.contains("acceptance_failed"),
            "{}",
            completed.output
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn complete_rechecks_stored_evidence() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "complete with evidence",
                "acceptance": ["cmd: cargo test"],
                "steps": [{"title": "change", "kind": "change"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        let mut journal = crate::agent::journal::Journal::open(&dir, "complete-rules").unwrap();
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        journal.set_attribution(Some("1".into()), Some(plan_id), "main");
        journal.append("plan", json!({"op": "start"})).unwrap();
        let evidence = journal
            .append_evidence("file_diff", json!({"path": "src/main.rs"}))
            .unwrap();
        let finished = plan_op(
            &mut ctx,
            &json!({"op": "finish", "id": "1", "summary": "changed", "evidence": [evidence]}),
        );
        assert!(finished.ok, "{}", finished.output);
        let complete = plan_op(&mut ctx, &json!({"op": "complete"}));
        assert!(!complete.ok);
        assert!(
            complete.output.contains("acceptance_pending"),
            "{}",
            complete.output
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
