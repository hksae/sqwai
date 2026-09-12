#![allow(dead_code)]
//! Built-in tool registry (phase 2).
//!
//! Each tool declares its JSON schema for the model and a handler. Handlers
//! receive a [`ToolCtx`] carrying the project root and session-scoped guard
//! state (which files were read, checkpoint journal).

mod astgrep;
mod exec;
mod fs;
mod git;
mod outline;
pub(crate) mod web;

use crate::agent::graph::GraphStore;
use crate::agent::safety;
use crate::plan;
use serde_json::{Value, json};
use std::collections::HashMap;
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
    /// Session this context belongs to. §2.5 keys the shadow snapshot chain
    /// on it (`refs/sessions/<id>`), so two sessions in one project do not
    /// interleave their checkpoint history — and retention can truncate one
    /// session's chain without touching another's.
    pub session_id: String,
    /// Files read this session and the content hash they had at the time,
    /// keyed by canonical path. §4 calls for the guard to be hash-tracked: a
    /// file changed by `bash` since the last read has to be read again, and
    /// with paths alone the model could edit it blind. The canonical key also
    /// stops `read("src/x.rs")` followed by `edit("./src/x.rs")` from being
    /// refused as unread.
    pub files_read: HashMap<PathBuf, String>,
    /// journal of checkpoints created by this session's mutations
    pub journal: Vec<(String, String)>,
    /// Host limits on the plan, and the model context they are derived from.
    /// The budget used to come from a `context_limit` the model passed in its
    /// own tool arguments (§2.1.2 makes it a host value).
    pub plan_limits: crate::config::PlanConfig,
    /// context window of the model driving this session, in tokens
    pub context_limit: u64,
    /// §3.7 (§7 S): set from the TUI when the user presses Esc during a
    /// running tool. Long-running handlers poll it at their existing
    /// wait/retry points and stop cooperatively — nothing here can reach into
    /// a handler and pull it out mid-syscall, so a handler that never checks
    /// this is a handler Esc cannot interrupt.
    pub cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Where the shadow repository lives, from [undo].shadow configuration.
    pub shadow_store: crate::config::ShadowStore,
    /// Step this session currently holds (§2.2.3). Set on `plan start`,
    /// cleared on `finish`/`block`/`cancel`; the loop mirrors it into the
    /// journal attribution at dispatch. `None` means idle.
    pub current_step: Option<String>,
    /// Immutable spawn context for subagent sessions (§2.2.4). `None` for
    /// the main agent. Mutating tools refuse to run when the inherited
    /// epoch no longer matches the plan.
    pub subagent_step: Option<plan::StepContext>,
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
            session_id: "shared".into(),
            files_read: HashMap::new(),
            journal: Vec::new(),
            plan_limits: crate::config::PlanConfig::default(),
            context_limit: 0,
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            shadow_store: crate::config::ShadowStore::Local,
            current_step: None,
            subagent_step: None,
        }
    }

    pub fn with_shadow_store(mut self, shadow_store: crate::config::ShadowStore) -> Self {
        self.shadow_store = shadow_store;
        self
    }

    /// Name the session whose checkpoint chain this context appends to.
    pub fn in_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = session_id.into();
        self
    }

    /// Share a cancellation flag across this context and its clones. The
    /// caller keeps the `Arc` and flips it; every clone taken afterwards
    /// (including the one moved into a `spawn_blocking` closure) observes it.
    pub fn with_cancel(mut self, cancel: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn cancel_requested(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Adopt the host's plan limits and the driving model's context window.
    pub fn with_plan_limits(
        mut self,
        plan_limits: crate::config::PlanConfig,
        context_limit: u64,
    ) -> Self {
        self.plan_limits = plan_limits;
        self.context_limit = context_limit;
        self
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

    fn read_key(p: &Path) -> PathBuf {
        p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
    }

    fn mark_read(&mut self, p: &Path) {
        let hash = file_hash(p);
        self.files_read.insert(Self::read_key(p), hash);
    }

    /// Whether the file may be edited: it was read, and it still holds what it
    /// held then.
    fn read_state(&self, p: &Path) -> ReadState {
        match self.files_read.get(&Self::read_key(p)) {
            None => ReadState::Unread,
            Some(seen) if *seen == file_hash(p) => ReadState::Current,
            Some(_) => ReadState::Stale,
        }
    }
}

/// What the read guard knows about a file the model wants to edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadState {
    /// never read in this session
    Unread,
    /// read, and unchanged since
    Current,
    /// read, but something changed it afterwards — `bash`, a formatter, the
    /// user's editor
    Stale,
}

/// Content hash of a file, matching what `file_diff` records. A missing or
/// unreadable file hashes to the empty string, which never equals a recorded
/// hash, so it reads as stale rather than as current.
fn file_hash(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            format!("{:x}", hasher.finalize())
        }
        Err(_) => String::new(),
    }
}

/// Smallest plan budget the host will use, however small the context. A plan
/// that cannot hold its own goal line is worse than an unbudgeted one.
pub(crate) const MIN_PLAN_BUDGET_TOKENS: u64 = 256;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    pub hash_before: Option<String>,
    pub hash_after: String,
    pub mode: String,
    pub checkpoint: Option<String>,
    /// blake3 name of the pre-image in the layer-1 blob store (§2.5), when it
    /// was stored. `None` for a file that did not exist yet, and for a store
    /// that could not be written — the edit still happens, uninsured, which
    /// is the same trade the git snapshot already made.
    pub blob_before: Option<String>,
    /// blake3 name of the content that was written
    pub blob_after: Option<String>,
}

pub struct Outcome {
    pub ok: bool,
    /// short result the model (and the collapsed TUI row) sees
    pub output: String,
    /// process exit code when this outcome came from a child process.
    /// `None` for host-side results (reads, listings, rejections) and for
    /// outcomes whose producer does not report one.
    pub exit_code: Option<i32>,
    /// unified diff of a file mutation, shown in the TUI when expanded
    pub diff: Option<String>,
    /// host-derived metadata for the journal
    pub file_diff: Option<FileDiff>,
    /// host-derived metadata for all modified files (e.g. multi-file patch)
    pub file_diffs: Vec<FileDiff>,
    /// §3.7: the user pressed Esc while this tool was running. Distinct from
    /// an ordinary failure — the journal records `code: "cancelled"` rather
    /// than folding it into an error the model is expected to react to.
    pub cancelled: bool,
}

impl Outcome {
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            ok: true,
            output: output.into(),
            exit_code: None,
            diff: None,
            file_diff: None,
            file_diffs: Vec::new(),
            cancelled: false,
        }
    }
    pub fn err(output: impl Into<String>) -> Self {
        Self {
            ok: false,
            output: output.into(),
            exit_code: None,
            diff: None,
            file_diff: None,
            file_diffs: Vec::new(),
            cancelled: false,
        }
    }
    /// §3.7: `tool_result ok:false code:cancelled`. The step stays
    /// `in_progress` and nothing prior is reverted — this only marks the one
    /// call that was interrupted.
    pub fn cancelled() -> Self {
        Self {
            ok: false,
            output: "cancelled by user (Esc)".to_string(),
            exit_code: None,
            diff: None,
            file_diff: None,
            file_diffs: Vec::new(),
            cancelled: true,
        }
    }
    /// attach the child exit code to a host-built outcome
    pub fn with_exit_code(mut self, code: Option<i32>) -> Self {
        self.exit_code = code;
        self
    }
    /// attach a unified diff, keeping the short summary
    pub fn with_diff(mut self, diff: String) -> Self {
        if !diff.is_empty() {
            self.diff = Some(diff);
        }
        self
    }

    pub fn with_file_diff(mut self, file_diff: FileDiff) -> Self {
        self.file_diff = Some(file_diff.clone());
        self.file_diffs.push(file_diff);
        self
    }

    pub fn with_file_diffs(mut self, file_diffs: Vec<FileDiff>) -> Self {
        if let Some(first) = file_diffs.first() {
            self.file_diff = Some(first.clone());
        }
        self.file_diffs.extend(file_diffs);
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
            name: "ast_grep",
            kind: Kind::ReadOnly,
            description: "Structural code search with AST patterns: finds code by shape, not text. \
$NAME captures one node (any kind or size), $$$NAME captures zero or more siblings, uppercase names are metavariables, the rest must match exactly; comments are ignored. \
Examples: `Ok($E)` finds every Ok(...) wrapping; `let $N = $V;` finds bindings; `f($$$ARGS)` finds calls with any argument list. \
Use instead of grep when whitespace, line breaks or comments vary.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "code pattern with $NAME / $$$NAME metavariables"},
                    "path": {"type": "string", "description": "file or directory, default project root"},
                    "lang": {"type": "string", "enum": ["rust", "python", "javascript", "typescript", "tsx", "go", "bash", "c", "cpp", "csharp", "java"], "description": "force a language; default is per-file by extension"},
                    "include": {"type": "string", "description": "path glob filter, e.g. src/**/*.rs"},
                    "max": {"type": "integer", "description": "max matches (default 50, max 200)"}
                },
                "required": ["pattern"]
            }),
        },
        ToolDef {
            name: "outline",
            kind: Kind::ReadOnly,
            description: "Structural outline of a source file showing functions, methods, classes, structs, enums, interfaces, and modules with line numbers. Use to quickly inspect file structure and find definitions before targeted reading. Supports 11 languages via AST (Rust, Python, JS, TS, Go, Bash, C, C++, C#, Java) and universal indentation/keyword fallback for other files.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "path to the source file"},
                    "max_depth": {"type": "integer", "description": "maximum nesting depth (default 2; 1 = top-level only, 2 = include methods/fields)"}
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "bash",
            kind: Kind::Mutating,
            description: "Run a shell command in the project directory. Destructive or risky commands \
(rm -rf, sudo, disk ops, force-push, etc.) require user approval and the model should avoid them. \
Long output is truncated to a tail and the full log path is returned. Use background=true when the \
command is expected to outlast the normal tool timeout or when useful independent work can \
continue; wait on it with bash_output(id, wait_secs) or sleep(seconds) instead of polling in a \
tight loop; await its result before dependent changes or reporting success.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "the shell command line to run"},
                    "timeout": {"type": "integer", "description": "seconds; kills the process on expiry"},
                    "background": {"type": "boolean", "description": "detach and return immediately with a job id; poll bash_output, stop bash_kill"}
                },
                "required": ["command"]
            }),
        },
        ToolDef {
            name: "bash_output",
            kind: Kind::ReadOnly,
            description: "Read a background command's output — incremental: the first read returns the tail, later reads return only the bytes appended since the previous read (from_start=true re-reads the tail). wait_secs (0-60) blocks until the job exits, fresh output arrives, or the timeout lapses: one call instead of a poll loop. A finished job is reported once with its exit code, then cleaned up. Without id: a list of all background jobs with their commands and log paths.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "job id from bash background=true"},
                    "tail": {"type": "integer", "description": "bytes of output to return (default 10000, max 50000)"},
                    "wait_secs": {"type": "integer", "description": "block up to N seconds (max 60) for exit or fresh output"},
                    "from_start": {"type": "boolean", "description": "re-read the tail from scratch instead of the delta"}
                }
            }),
        },
        ToolDef {
            name: "bash_kill",
            kind: Kind::ReadOnly,
            description: "Stop a background command started with bash background=true, including everything it spawned. Reports the job's command and final state.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "job id"}
                },
                "required": ["id"]
            }),
        },
        ToolDef {
            name: "sleep",
            kind: Kind::ReadOnly,
            description: "Wait N seconds (0-60, clamped) without doing anything: for pauses a file, a server, or a human needs. Esc cancels the wait. For background jobs prefer bash_output(id, wait_secs): it wakes on fresh output instead of sleeping blind.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "seconds": {"type": "integer", "description": "how long to wait (0-60)"}
                },
                "required": ["seconds"]
            }),
        },
        ToolDef {
            name: "think",
            kind: Kind::ReadOnly,
            description: "A scratchpad with no effects: record a short approach and the checks that will validate it when that reasoning is worth keeping in history. Routine reasoning stays in the reply. Returns ok; the value is the reasoning itself.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "thought": {"type": "string", "description": "the reasoning to write down"}
                },
                "required": ["thought"]
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
            name: "step_diff",
            kind: Kind::ReadOnly,
            description: "Show what changed in a specific plan step by comparing shadow checkpoint boundaries. Answers what was modified during that step.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "step_id": {
                        "type": "string",
                        "description": "plan step id (e.g. '1', '2')"
                    },
                    "path": {
                        "type": "string",
                        "description": "optional path to restrict the diff to"
                    }
                },
                "required": ["step_id"]
            }),
        },
        ToolDef {
            name: "git_log",
            kind: Kind::ReadOnly,
            description: "Show recent Git commits.",
            parameters: json!({"type":"object","properties":{"count":{"type":"integer","minimum":1,"maximum":100},"format":{"type":"string"}}}),
        },
        ToolDef {
            name: "git_show",
            kind: Kind::ReadOnly,
            description: "Show what a commit changed, or what a file looked like at a revision. With path: the file's content at the revision. Without: the commit message, diff stat and patch.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "commit": {"type": "string", "description": "revision (sha, branch, HEAD~N); default HEAD"},
                    "path": {"type": "string", "description": "repo-relative file path (forward slashes)"}
                }
            }),
        },
        ToolDef {
            name: "git_commit",
            kind: Kind::Mutating,
            description: "Create a Git commit from currently staged changes, or all tracked changes when all is true.",
            parameters: json!({"type":"object","properties":{"message":{"type":"string"},"all":{"type":"boolean"}},"required":["message"]}),
        },
        ToolDef {
            name: "git_stage",
            kind: Kind::Mutating,
            description: "Stage or unstage file changes in the Git index (including untracked files).",
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["add", "reset"],
                        "description": "'add' to stage changes, 'reset' to unstage (default 'add')"
                    },
                    "paths": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Repo-relative file paths or glob patterns"
                    },
                    "all": {
                        "type": "boolean",
                        "description": "Stage or reset all files (equivalent to git add -A / git reset)"
                    }
                }
            }),
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
            description: "Fetch a bounded HTTP(S) page or text response and return readable text. Use only user-provided or task-relevant URLs. On HTML pages pass a CSS selector to extract just the matching elements instead of the whole page.",
            parameters: json!({"type":"object","properties":{"url":{"type":"string"},"selector":{"type":"string","description":"CSS selector: return only the text of matching elements (HTML pages only)"},"timeout":{"type":"integer","minimum":1,"maximum":60}},"required":["url"]}),
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
            parameters: json!({"type":"object","properties":{"note":{"type":"string"},"kind":{"type":"string","enum":["decision","rejected","assumption","lesson","blocker"]},"resolves":{"type":"integer","description":"journal seq of an assumption this note closes (§2.1.4)"}},"required":["note","kind"]}),
        },
        ToolDef {
            name: "journal",
            kind: Kind::ReadOnly,
            description: "Read the host journal: the factual event log of what happened in this \
             project (user messages, tool calls and results, file diffs, plan ops, notes, \
             checkpoints). Every line carries j#<seq>, the stable reference used by plan \
             evidence and note resolves. Times are UTC. Use it to answer questions about \
             past actions, find which evidence exists for a step, or recall what was already \
             tried. Output is newest-tail first narrowed by filters and always capped: \
             narrow with kind/step/from/to/after/query instead of dumping everything.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "op": {"type": "string", "enum": ["read", "assumptions"], "description": "read journal records (default) or list open assumptions"},
                    "session": {"type": "string", "enum": ["current", "all"], "description": "whose journal to read: this session (default) or every session in the project"},
                    "kind": {"type": "string", "description": "exact record kind: user_msg|tool_call|tool_result|file_diff|diagnostics|note|plan|checkpoint|provider_error|compaction"},
                    "step": {"type": "string", "description": "only records attached to this plan step id"},
                    "from": {"type": "string", "description": "inclusive lower time bound, UTC: RFC3339 or YYYY-MM-DD"},
                    "to": {"type": "string", "description": "inclusive upper time bound, UTC: RFC3339 or YYYY-MM-DD (a bare date means through the end of that day)"},
                    "after": {"type": "integer", "description": "only records with j# greater than this (paging)"},
                    "last": {"type": "integer", "description": "max records to return, default 40, maximum 200"},
                    "query": {"type": "string", "description": "case-insensitive substring matched against the rendered line"}
                }
            }),
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
            name: "resolve_ref",
            kind: Kind::ReadOnly,
            description: "Resolve a code reference (file path and/or symbol name) against the project graph. \
Guarantees disk freshness by verifying file byte hash before resolution. \
Returns definition location, signature, provenance (source_hash, generation, freshness, precision), and capabilities, or suggestions if not found.",
            parameters: json!({
                "type": "object",
                "properties": {
                "ref": {
                    "type": "string",
                    "description": "reference key (sym:path::kind::name) or shorthand (path::symbol)"
                },
                "path": {
                    "type": "string",
                    "description": "project-relative file path"
                },
                "symbol": {
                    "type": "string",
                    "description": "symbol name or scoped name"
                }
            }}),
        },
        ToolDef {
            name: "recall",
            kind: Kind::ReadOnly,
            description: "Search code and memory graph by symbol name, path, concept, or memory text snippet using deterministic ranking. \
Returns matching items with canonical keys (sym:..., file:..., mem:...), kinds, paths, one-line snippets, and provenance. \
Always prefer using the canonical keys returned by recall in subsequent graph_query calls.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "search query (symbol, path, concept, or memory text)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "maximum results to return (default 8, max 20)"
                    }
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "graph_query",
            kind: Kind::ReadOnly,
            description: "Traverse relationships in the code and memory graph from a starting node using bounded breadth-first search. \
Accepts canonical keys (sym:..., file:..., mem:...) or shorthand (path::symbol, symbol name). \
If the starting node is unresolvable or ambiguous, returns an explicit error with candidates (use recall to find canonical keys). \
By default, uses the 'dependencies' preset and does not expand file containers into sibling declarations. \
Returns connected nodes, incident edges, and explicit truncation status.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "node": {
                        "type": "string",
                        "description": "canonical node key (sym:..., file:..., mem:...) or shorthand (path::symbol, symbol name)"
                    },
                    "preset": {
                        "type": "string",
                        "enum": ["dependencies", "structure", "related_notes", "all"],
                        "description": "relation preset: 'dependencies' (calls, imports, references, about; default), 'structure' (hierarchy/containment), 'related_notes' (memories/decisions), 'all'"
                    },
                    "direction": {
                        "type": "string",
                        "enum": ["both", "incoming", "outgoing"],
                        "description": "traversal direction (default 'both')"
                    },
                    "max_depth": {
                        "type": "integer",
                        "description": "traversal depth 1..=3 (default 2)"
                    },
                    "max_nodes": {
                        "type": "integer",
                        "description": "maximum node budget 1..=100 (default 30)"
                    },
                    "max_edges": {
                        "type": "integer",
                        "description": "maximum edge budget 1..=100 (default 50)"
                    },
                    "max_output_tokens": {
                        "type": "integer",
                        "description": "maximum token budget for formatted output (default 2000)"
                    },
                    "relations": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "custom edge kind filter overriding preset (e.g. ['calls', 'imports'])"
                    },
                    "kinds": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "optional node kind filter (e.g. ['function', 'struct', 'decision'])"
                    }
                },
                "required": ["node"]
            }),
        },
        ToolDef {
            name: "propose_plan",
            kind: Kind::ReadOnly,
            description: "Propose a new full plan or a replacement for the active one. \
             Nothing is written until the user accepts: the host validates the draft first \
             (format errors reject this call without bothering the user), then shows it \
             for accept/decline with a preview. If declined, the outcome says so — ask \
             the user what was wrong and adjust, do not stop. Use for a new goal and for \
             replacing the active plan; small edits to the active plan use plan add/split.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "goal": {"type": "string", "description": "what must be true when the work is done"},
                    "constraints": {"type": "array", "items": {"type": "string"}},
                    "acceptance": {"type": "array", "items": {"type": "string"}},
                    "steps": {
                        "type": "array",
                        "description": "initial steps (3-12)",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": {"type": "string"},
                                "kind": {"type": "string", "enum": ["research", "change", "verify"]},
                                "refs": {"type": "array", "items": {"type": ["string", "object"]}, "description": "what the step touches: plain \"path[::symbol]\" means modify, or {\"path\", \"symbol\", \"intent\": \"modify|create|remove\"}"}
                            },
                            "required": ["title"]
                        }
                    }
                },
                "required": ["goal", "steps"]
            }),
        },
        ToolDef {
            name: "plan",
            kind: Kind::Mutating,
            description: "Work the structured plan, one operation per call. Ops: create, start, \
finish, block, unblock, cancel, add, split, verify, complete, show. Call \
show first if you are unsure of the current step ids. The host owns the goal, the constraints, \
acceptance status, validation and evidence; to change the goal, propose the full updated plan with \
propose_plan instead. finish records completion of the step's work with a summary and does not \
by itself establish that acceptance criteria passed; it requires host-recorded evidence since \
start (research: a tool_result, change: a file_diff, verify: successful observed execution or \
clean diagnostics) and rejections return a code and hint to follow. A manual: acceptance can be \
waived only by the user, never verified by the model. complete requires every step closed and \
every acceptance validation passed or waived with fresh receipts. Never invent evidence identifiers.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "op": {"type": "string", "enum": [
                        "create", "start", "finish", "block", "unblock", "cancel",
                        "add", "split", "verify", "complete", "show"
                    ]},
                    "id": {"type": "string", "description": "step id"},
                    "goal": {"type": "string", "description": "create"},
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
                                "refs": {"type": "array", "items": {"type": ["string", "object"]}, "description": "what the step touches: plain \"path[::symbol]\" means modify, or {\"path\", \"symbol\", \"intent\": \"modify|create|remove\"}"}
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
                                "refs": {"type": "array", "items": {"type": ["string", "object"]}, "description": "what the step touches: plain \"path[::symbol]\" means modify, or {\"path\", \"symbol\", \"intent\": \"modify|create|remove\"}"}
                            },
                            "required": ["title"]
                        }
                    },
                    "after": {"type": "string", "description": "add: insert after this step id"},
                    "title": {"type": "string", "description": "add: new step title"},
                    "kind": {"type": "string", "enum": ["research", "change", "verify"]},
                    "refs": {"type": "array", "items": {"type": ["string", "object"]}, "description": "what the step touches: plain \"path[::symbol]\" means modify, or {\"path\", \"symbol\", \"intent\": \"modify|create|remove\"}"},
                    "summary": {"type": "string", "description": "finish: what was done, where, and any remaining limitations"},
                    "reason": {"type": "string", "description": "block / cancel"},
                    "confirm": {"type": "boolean", "description": "start: re-read a stale step"},
                    "evidence": {"type": "array", "items": {"type": "integer"}, "description": "deprecated informational field; host ignores it"}
                },
                "required": ["op"]
            }),
        },
        ToolDef {
            name: "ask_user",
            kind: Kind::ReadOnly,
            description: "Ask the user structured questions (1-4) with 2-5 answer options each. Use only for decisions that materially change the outcome (approach, library, schema), never for trivial clarification. The user can pick options (single or multiple per question) and/or type a custom answer per question.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string", "description": "single question (legacy, use questions for multiple)"},
                    "questions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "header": {"type": "string", "description": "Very short label (max 30 chars)"},
                                "question": {"type": "string", "description": "Complete question"},
                                "options": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": {"type": "string", "description": "Display text (1-5 words, concise)"},
                                            "description": {"type": "string", "description": "Explanation of choice"}
                                        },
                                        "required": ["label"]
                                    },
                                    "minItems": 2,
                                    "maxItems": 5
                                },
                                "multiple": {"type": "boolean", "description": "Allow selecting multiple choices"},
                                "allow_free": {"type": "boolean", "description": "Allow a custom typed answer as an extra option"}
                            },
                            "required": ["question", "options"]
                        },
                        "minItems": 1,
                        "maxItems": 4
                    },
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
                    "multiple": {"type": "boolean", "description": "allow selecting several options (single-question mode)"},
                    "allow_free": {"type": "boolean", "description": "allow a custom typed answer (single-question mode)"}
                },
                "anyOf": [
                    {"required": ["questions"]},
                    {"required": ["question", "options"]}
                ]
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
        "bash_output" => args["id"]
            .as_u64()
            .map(|id| format!("job {id}"))
            .unwrap_or_else(|| "list".to_string()),
        "bash_kill" => format!("job {}", args["id"].as_u64().unwrap_or(0)),
        "sleep" => format!("{}s", args["seconds"].as_u64().unwrap_or(0)),
        "think" => {
            let thought: String = s("thought").chars().take(80).collect();
            thought
        }
        "git_show" => {
            let commit = s("commit");
            let path = s("path");
            if path.is_empty() {
                if commit.is_empty() {
                    "HEAD".to_string()
                } else {
                    commit
                }
            } else if commit.is_empty() {
                format!("HEAD:{path}")
            } else {
                format!("{commit}:{path}")
            }
        }
        "glob" | "grep" => s("pattern"),
        "ast_grep" => {
            let pattern = clip(&s("pattern"), 60);
            let lang = s("lang");
            if lang.is_empty() {
                pattern
            } else {
                format!("{pattern} @{lang}")
            }
        }
        "outline" => {
            let path = s("path");
            if let Some(d) = args["max_depth"].as_u64() {
                format!("{path} depth={d}")
            } else {
                path
            }
        }
        "git_diff" => s("target"),
        "step_diff" => {
            let step = s("step_id");
            let path = s("path");
            if path.is_empty() {
                format!("step {step}")
            } else {
                format!("step {step} {path}")
            }
        }
        "git_commit" => s("message"),
        "git_stage" => {
            let action = match args["action"].as_str() {
                Some("reset") => "reset",
                _ => "add",
            };
            if args["all"].as_bool().unwrap_or(false) {
                format!("{action} all")
            } else if let Some(arr) = args["paths"].as_array() {
                format!("{action} {} paths", arr.len())
            } else if let Some(p) = args["path"].as_str() {
                format!("{action} {p}")
            } else {
                action.to_string()
            }
        }
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
        "ask_user" => {
            let single = s("question");
            if !single.is_empty() {
                return single;
            }
            // multi-question mode: compact one-line summary for the chat row
            // and the history expansion (live asks use the inline segment)
            let Some(qs) = args.get("questions").and_then(|v| v.as_array()) else {
                return String::new();
            };
            let mut parts = Vec::new();
            for q in qs {
                let qq = q.get("question").and_then(|v| v.as_str()).unwrap_or("");
                if qq.is_empty() {
                    continue;
                }
                let header = q.get("header").and_then(|v| v.as_str()).unwrap_or("");
                if header.is_empty() {
                    parts.push(qq.to_string());
                } else {
                    parts.push(format!("{header}: {qq}"));
                }
            }
            if parts.len() > 2 {
                format!("{} (+{} more)", parts[..2].join(" | "), parts.len() - 2)
            } else {
                parts.join(" | ")
            }
        }
        "plan" => format!("plan {}", s("op")),
        "propose_plan" => s("goal"),
        "resolve_ref" => {
            if let Some(r) = args["ref"].as_str() {
                format!("resolve_ref {r}")
            } else if let Some(p) = args["path"].as_str() {
                let sym = args["symbol"].as_str().unwrap_or("*");
                format!("resolve_ref {p}::{sym}")
            } else if let Some(s) = args["symbol"].as_str() {
                format!("resolve_ref {s}")
            } else {
                "resolve_ref".to_string()
            }
        }
        "recall" => format!("recall {}", s("query")),
        "graph_query" => {
            let node = s("node");
            let dir = args["direction"].as_str().unwrap_or("both");
            format!("graph_query {node} ({dir})")
        }
        "journal" => {
            let op = args["op"].as_str().unwrap_or("read");
            if op == "assumptions" {
                return "assumptions".to_string();
            }
            let mut parts: Vec<String> = Vec::new();
            for key in ["kind", "step", "query", "from", "to", "session"] {
                if let Some(v) = args[key].as_str() {
                    parts.push(format!("{key}={v}"));
                }
            }
            if args["after"].as_u64().is_some() {
                parts.push("after".to_string());
            }
            if parts.is_empty() {
                "recent records".to_string()
            } else {
                parts.join(" ")
            }
        }
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
        .map(|d| {
            let mut spec = crate::providers::ToolSpec {
                name: d.name.to_string(),
                description: d.description.to_string(),
                parameters: d.parameters,
            };
            // `git_branch` is read-only as a tool but its `create` and
            // `switch` actions are not, and the dispatcher refuses them in
            // PLAN mode. Advertising them anyway costs a turn to find that
            // out, so the schema says what the mode allows.
            if plan_mode && spec.name == "git_branch" {
                spec.parameters["properties"]["action"]["enum"] = json!(["list", "current"]);
                spec.description =
                    "List local branches, or show the current one. Creating and switching \
                     branches is an ACT-mode action."
                        .to_string();
            }
            spec
        })
        .collect();
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    specs
}

const READ_MAX_BYTES: usize = 400_000;

/// True when the step a subagent was spawned for still exists in the named
/// plan at exactly the inherited epoch (§2.2.4). Anything else — reopened,
/// retired plan, deleted step — means the inherited context is stale.
fn step_epoch_current(root: &Path, inherited: &plan::StepContext) -> bool {
    let Some(plan) = plan::read_plan_file(root, &inherited.plan_id) else {
        return false;
    };
    plan.steps
        .iter()
        .find(|step| step.id == inherited.step_id)
        .is_some_and(|step| step.step_epoch == inherited.step_epoch)
}

/// dispatch one tool call
pub fn execute(ctx: &mut ToolCtx, name: &str, args: &Value) -> Outcome {
    if ctx.read_only
        && matches!(
            name,
            "write"
                | "edit"
                | "multi_edit"
                | "git_commit"
                | "git_stage"
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
    // A subagent mutating after its step was reopened (or its plan retired)
    // would attach stale work to a fresh epoch (§2.2.4). Refuse instead.
    if let Some(inherited) = ctx.subagent_step.clone()
        && matches!(name, "write" | "edit" | "multi_edit" | "patch" | "bash")
        && !step_epoch_current(&ctx.root, &inherited)
    {
        return Outcome::err(
            serde_json::json!({
                "ok": false,
                "code": "stale_epoch",
                "reason": format!(
                    "step {} was reopened or retired after this task was spawned (inherited epoch {})",
                    inherited.step_id, inherited.step_epoch,
                ),
                "hint": "stop working on this step; report what was done before the reopen",
            })
            .to_string(),
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
        "step_diff" => git::step_diff(ctx, args),
        "git_log" => git::log(ctx, args),
        "git_show" => git::show(ctx, args),
        "git_commit" => git::commit(ctx, args),
        "git_stage" => git::stage(ctx, args),
        "git_branch" => git::branch(ctx, args),
        "patch" => git::patch(ctx, args),
        "ast_grep" => astgrep::ast_grep(ctx, args),
        "outline" => outline::outline(ctx, args),
        "webfetch" | "websearch" => Outcome::err("web tools must run through the async dispatcher"),
        "bash" => exec::bash(
            ctx,
            args["command"].as_str().unwrap_or_default(),
            args["timeout"].as_u64(),
            args["background"].as_bool().unwrap_or(false),
        ),
        "bash_output" => exec::bash_output(ctx, args),
        "bash_kill" => exec::bash_kill(args),
        "sleep" => exec::sleep(ctx, args),
        "think" => Outcome::ok("ok — continue with the next step of your plan."),
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
        "resolve_ref" => {
            let raw_ref = args["ref"].as_str();
            let path = args["path"].as_str();
            let symbol = args["symbol"].as_str();
            if raw_ref.is_none() && path.is_none() && symbol.is_none() {
                return Outcome::err("resolve_ref requires 'ref', 'path', or 'symbol'");
            }
            let mut store = match crate::agent::graph::SqliteGraphStore::open(&ctx.root) {
                Ok(s) => s,
                Err(e) => return Outcome::err(format!("cannot open graph: {e:#}")),
            };
            match store.resolve_ref(raw_ref, path, symbol) {
                Ok(res) => Outcome::ok(serde_json::to_string_pretty(&res).unwrap_or_default()),
                Err(e) => Outcome::err(format!("resolve_ref failed: {e:#}")),
            }
        }
        "recall" => {
            let query = match args["query"].as_str() {
                Some(q) if !q.trim().is_empty() => q.trim(),
                _ => return Outcome::err("recall requires a non-empty 'query' argument"),
            };
            let limit = args["limit"].as_u64().unwrap_or(8) as usize;
            let store = match crate::agent::graph::SqliteGraphStore::open(&ctx.root) {
                Ok(s) => s,
                Err(e) => return Outcome::err(format!("cannot open graph: {e:#}")),
            };
            match store.recall(query, limit) {
                Ok(items) => {
                    if items.is_empty() {
                        Outcome::ok(format!("No recall matches found for '{query}'."))
                    } else {
                        let mut out = format!("Recall results for '{query}' ({} matches):\n", items.len());
                        for (i, item) in items.iter().enumerate() {
                            out.push_str(&format!(
                                "{}. [{}] {} (score: {:.2})\n   snippet: {}\n",
                                i + 1,
                                item.kind,
                                item.key,
                                item.score,
                                item.snippet
                            ));
                            if let Some(author) = &item.author {
                                out.push_str(&format!("   author: {author}"));
                                if let Some(jref) = &item.journal_ref {
                                    out.push_str(&format!(" ({jref})"));
                                }
                                out.push('\n');
                            }
                        }
                        Outcome::ok(out)
                    }
                }
                Err(e) => Outcome::err(format!("recall failed: {e:#}")),
            }
        }
        "graph_query" => {
            let node = match args["node"].as_str() {
                Some(n) if !n.trim().is_empty() => n.trim(),
                _ => return Outcome::err("graph_query requires a non-empty 'node' argument"),
            };
            let preset = args["preset"].as_str().map(String::from);
            let dir_str = args["direction"].as_str().unwrap_or("both");
            let direction = match dir_str.to_ascii_lowercase().as_str() {
                "in" | "incoming" => crate::agent::graph::Direction::Incoming,
                "out" | "outgoing" => crate::agent::graph::Direction::Outgoing,
                _ => crate::agent::graph::Direction::Both,
            };
            let depth = args["max_depth"]
                .as_u64()
                .or_else(|| args["depth"].as_u64())
                .unwrap_or(crate::agent::graph::DEFAULT_MAX_DEPTH as u64)
                .clamp(1, 3) as u8;
            let max_nodes = args["max_nodes"]
                .as_u64()
                .unwrap_or(crate::agent::graph::DEFAULT_MAX_NODES as u64)
                .clamp(1, 100) as usize;
            let max_edges = args["max_edges"]
                .as_u64()
                .or_else(|| args["limit"].as_u64())
                .unwrap_or(crate::agent::graph::DEFAULT_MAX_EDGES as u64)
                .clamp(1, 100) as usize;
            let max_output_tokens = args["max_output_tokens"]
                .as_u64()
                .unwrap_or(2000) as usize;
            let relations: Vec<String> = args["relations"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let kinds: Vec<String> = args["kinds"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            let store = match crate::agent::graph::SqliteGraphStore::open(&ctx.root) {
                Ok(s) => s,
                Err(e) => return Outcome::err(format!("cannot open graph: {e:#}")),
            };
            match store.graph_query(
                node,
                crate::agent::graph::GraphQuery {
                    direction,
                    preset: preset.clone(),
                    depth,
                    max_nodes,
                    max_edges,
                    limit: max_edges,
                    relations,
                    kinds,
                },
            ) {
                Ok(proj) => {
                    let preset_label = preset.as_deref().unwrap_or("dependencies");
                    let mut out = format!(
                        "Graph query for '{node}': {} nodes, {} edges (preset={preset_label}, depth={depth}, max_nodes={max_nodes}, max_edges={max_edges})\n",
                        proj.nodes.len(),
                        proj.edges.len()
                    );
                    if proj.truncated {
                        out.push_str(&format!(
                            "[truncated: {}]\n",
                            proj.truncated_reason.as_deref().unwrap_or("budget reached")
                        ));
                    }
                    out.push_str("Nodes:\n");
                    for n in &proj.nodes {
                        out.push_str(&format!(
                            "  - [{}] {}",
                            crate::agent::graph::node_kind_name(&n.kind),
                            n.stable_key
                        ));
                        if let Some(name) = &n.name {
                            out.push_str(&format!(" ({name})"));
                        }
                        if let Some(text) = n.properties.get("text").and_then(|v| v.as_str()) {
                            let preview = text.lines().next().unwrap_or("");
                            if !preview.is_empty() {
                                out.push_str(&format!(": \"{preview}\""));
                            }
                        }
                        out.push('\n');
                    }
                    out.push_str("Edges:\n");
                    for e in &proj.edges {
                        out.push_str(&format!(
                            "  - {} --({})--> {}\n",
                            e.from, e.kind, e.to
                        ));
                    }

                    // Token / character budget truncation
                    let max_chars = max_output_tokens * 4;
                    if out.len() > max_chars {
                        let cut = out.floor_char_boundary(max_chars);
                        out.truncate(cut);
                        out.push_str(&format!(
                            "\n[truncated: output budget reached ({max_output_tokens} tokens); remaining output omitted]\n"
                        ));
                    }
                    Outcome::ok(out)
                }
                Err(e) => {
                    let err_str = format!("{e:#}");
                    if err_str.contains("unresolved_start:") {
                        let hint = err_str.strip_prefix("unresolved_start:").unwrap_or(&err_str).trim();
                        Outcome::err(serde_json::to_string_pretty(&serde_json::json!({
                            "ok": false,
                            "code": "unresolved_start",
                            "node": node,
                            "hint": hint,
                        })).unwrap_or_else(|_| format!("unresolved start node '{node}': {hint}")))
                    } else if err_str.contains("ambiguous_start:") {
                        let hint = err_str.strip_prefix("ambiguous_start:").unwrap_or(&err_str).trim();
                        Outcome::err(serde_json::to_string_pretty(&serde_json::json!({
                            "ok": false,
                            "code": "ambiguous_start",
                            "node": node,
                            "hint": hint,
                        })).unwrap_or_else(|_| format!("ambiguous start node '{node}': {hint}")))
                    } else {
                        Outcome::err(format!("graph_query failed: {e:#}"))
                    }
                }
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
            } else if let Some(resolves) = args.get("resolves").and_then(Value::as_u64) {
                // Closing an assumption is only meaningful against one that is
                // actually open: a `resolves` pointing anywhere else would look
                // like closure while leaving the assumption standing.
                let open = crate::agent::journal::Journal::open_assumptions(&ctx.root, None)
                    .unwrap_or_default();
                if open.iter().any(|item| item.seq == resolves) {
                    Outcome::ok(format!("note recorded: {kind}, resolves j#{resolves}"))
                } else {
                    Outcome::err(format!(
                        "j#{resolves} is not an open assumption — call plan show or note without \
                         `resolves` to record this on its own"
                    ))
                }
            } else {
                Outcome::ok(format!("note recorded: {kind}"))
            }
        }
        // direct dispatch never answers "unknown tool"
        "ask_user" => Outcome::err("ask_user is served by the agent loop, not by the dispatcher"),
        "propose_plan" => {
            Outcome::err("propose_plan is served by the agent loop, not by the dispatcher")
        }
        "journal" => journal_op(ctx, args),
        other => Outcome::err(format!("unknown tool '{other}'")),
    }
}

/// The `journal` tool: a read-only projection of the host journal (§2.2).
///
/// The model never writes here except through `note`, and it cannot read the
/// journal files directly (host-owned state), so this op is the one window on
/// past actions. Everything it returns was screened when it was appended; the
/// output is filtered and capped so a broad query cannot flood the context.
fn journal_op(ctx: &mut ToolCtx, args: &Value) -> Outcome {
    let op = args["op"].as_str().unwrap_or("read");
    if op == "assumptions" {
        let open = match crate::agent::journal::Journal::open_assumptions(&ctx.root, None) {
            Ok(open) => open,
            Err(e) => return Outcome::err(format!("journal read failed: {e:#}")),
        };
        if open.is_empty() {
            return Outcome::ok("no open assumptions");
        }
        let lines: Vec<String> = open.iter().map(|a| a.label(160)).collect();
        return Outcome::ok(format!(
            "open assumptions (j# = journal seq):\n{}",
            lines.join("\n")
        ));
    }
    if op != "read" {
        return Outcome::err("journal op must be 'read' or 'assumptions'");
    }

    let (records, label) = match args["session"].as_str().unwrap_or("current") {
        "current" | "" => (
            crate::agent::journal::Journal::records_for(&ctx.root, &ctx.session_id),
            ctx.session_id.clone(),
        ),
        "all" => (
            crate::agent::journal::Journal::records(&ctx.root),
            "all sessions".to_string(),
        ),
        other => {
            // a session id is a bare file stem, never a path
            if other.contains('/') || other.contains('\\') || other.contains("..") {
                return Outcome::err(format!("bad session id '{other}'"));
            }
            (
                crate::agent::journal::Journal::records_for(&ctx.root, other),
                other.to_string(),
            )
        }
    };
    let records = match records {
        Ok(records) => records,
        Err(e) => return Outcome::err(format!("journal read failed: {e:#}")),
    };
    let total = records.len();
    let mut records = records;
    // records() walks session files in filesystem order; chronological order
    // is the only sane reading order once more than one session is involved.
    records.sort_by(|a, b| a.ts.cmp(&b.ts).then(a.seq.cmp(&b.seq)));

    let from = match args["from"].as_str() {
        Some(s) => match parse_time_bound(s, false) {
            Some(t) => Some(t),
            None => return Outcome::err("bad 'from': use RFC3339 or YYYY-MM-DD (UTC)"),
        },
        None => None,
    };
    let to = match args["to"].as_str() {
        Some(s) => match parse_time_bound(s, true) {
            Some(t) => Some(t),
            None => return Outcome::err("bad 'to': use RFC3339 or YYYY-MM-DD (UTC)"),
        },
        None => None,
    };
    let kind = args["kind"].as_str();
    let step = args["step"].as_str();
    let after = args["after"].as_u64();
    let query = args["query"].as_str().map(str::to_lowercase);

    let rendered: Vec<String> = records
        .iter()
        .filter(|r| kind.is_none_or(|k| r.kind == k))
        .filter(|r| step.is_none_or(|s| r.step.as_deref() == Some(s)))
        .filter(|r| after.is_none_or(|a| r.seq > a))
        .filter(|r| match (from, to) {
            (None, None) => true,
            _ => match chrono::DateTime::parse_from_rfc3339(&r.ts) {
                Ok(t) => from.is_none_or(|f| t >= f) && to.is_none_or(|t2| t <= t2),
                Err(_) => false,
            },
        })
        .map(journal_line)
        .filter(|line| {
            query
                .as_ref()
                .is_none_or(|q| line.to_lowercase().contains(q.as_str()))
        })
        .collect();
    let matched = rendered.len();
    if matched == 0 {
        return Outcome::ok(format!(
            "journal {label}: {total} records, 0 match the filters"
        ));
    }

    let last = args["last"].as_u64().unwrap_or(40).clamp(1, 200) as usize;
    let start = matched.saturating_sub(last);
    const OUTPUT_BUDGET: usize = 20_000;
    let mut lines: Vec<&String> = Vec::new();
    let mut used = 0usize;
    for line in rendered[start..].iter().rev() {
        let cost = line.len() + 1;
        if used + cost > OUTPUT_BUDGET && !lines.is_empty() {
            break;
        }
        used += cost;
        lines.push(line);
    }
    let dropped_oldest = matched - start - lines.len();
    lines.reverse();

    let mut out = format!(
        "journal {label}: {total} records total, {matched} match, showing {} (oldest first, UTC)",
        lines.len()
    );
    if start > 0 {
        out.push_str(&format!(
            "; {start} older matches — page with 'after' or raise 'last'"
        ));
    }
    if dropped_oldest > 0 {
        out.push_str(&format!(
            "; {dropped_oldest} oldest dropped (20 KB output cap)"
        ));
    }
    out.push('\n');
    out.push_str(
        &lines
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    );
    Outcome::ok(out)
}

/// One rendered journal line: `j#<seq> <time> <kind> [step=N] ([agent]) | body`.
/// Everything after `|` is a kind-specific summary with a generic k=v fallback.
fn journal_line(r: &crate::agent::journal::Record) -> String {
    let when = chrono::DateTime::parse_from_rfc3339(&r.ts)
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|_| r.ts.clone());
    let mut line = format!("j#{} {when} {}", r.seq, r.kind);
    if let Some(step) = &r.step {
        line.push_str(&format!(" step={step}"));
    }
    if r.agent != "main" {
        line.push_str(&format!(" [{}]", r.agent));
    }
    let body = clip(&journal_body(r), 140);
    if !body.is_empty() {
        line.push_str(" | ");
        line.push_str(&body);
    }
    line
}

/// Kind-specific one-line summary of a record's payload fields.
fn journal_body(r: &crate::agent::journal::Record) -> String {
    let f = &r.fields;
    let s = |k: &str| f.get(k).and_then(Value::as_str);
    let n = |k: &str| f.get(k).and_then(Value::as_u64);
    match r.kind.as_str() {
        "tool_call" => match (s("tool"), s("args_digest")) {
            (Some(tool), Some(digest)) => format!("{tool}({digest})"),
            (Some(tool), None) => tool.to_string(),
            _ => String::new(),
        },
        "tool_result" => {
            let mut parts: Vec<String> = Vec::new();
            if let Some(tool) = s("tool") {
                parts.push(tool.to_string());
            }
            if let Some(v) = f.get("ok") {
                parts.push(format!("ok={v}"));
            }
            if let Some(code) = s("code") {
                parts.push(format!("code={code}"));
            }
            parts.join(" ")
        }
        "file_diff" => {
            let mut parts: Vec<String> = Vec::new();
            if let Some(path) = s("path") {
                parts.push(path.to_string());
            }
            if let Some(v) = n("added") {
                parts.push(format!("+{v}"));
            }
            if let Some(v) = n("removed") {
                parts.push(format!("-{v}"));
            }
            parts.join(" ")
        }
        "diagnostics" => match s("path") {
            Some(path) => format!(
                "{path} errors={} warnings={} {}",
                n("errors").unwrap_or(0),
                n("warnings").unwrap_or(0),
                s("server").unwrap_or("")
            )
            .trim_end()
            .to_string(),
            None => String::new(),
        },
        "note" => {
            let mut out = match (s("note"), s("text")) {
                (Some(kind), Some(text)) => format!("{kind}: {text}"),
                _ => s("text").unwrap_or_default().to_string(),
            };
            if let Some(resolves) = n("resolves") {
                out.push_str(&format!(" (resolves j#{resolves})"));
            }
            out
        }
        "plan" => {
            let mut parts: Vec<String> = Vec::new();
            if let Some(v) = s("op") {
                parts.push(v.to_string());
            }
            for key in ["id", "goal", "step", "title", "result"] {
                if let Some(v) = s(key) {
                    parts.push(v.to_string());
                }
            }
            parts.join(" ")
        }
        "checkpoint" => s("label").unwrap_or_default().to_string(),
        "provider_error" => {
            let mut parts: Vec<String> = Vec::new();
            if let Some(class) = s("class") {
                parts.push(class.to_string());
            }
            if let Some(v) = n("retries") {
                parts.push(format!("retries={v}"));
            }
            if let Some(v) = f.get("recovered") {
                parts.push(format!("recovered={v}"));
            }
            parts.join(" ")
        }
        "user_msg" => match n("chars") {
            Some(chars) => format!("{} chars", chars),
            None => String::new(),
        },
        "compaction" => s("phase").unwrap_or_default().to_string(),
        "resume" => s("notice").unwrap_or_default().to_string(),
        _ => {
            // generic fallback: a few short k=v pairs
            let mut parts: Vec<String> = Vec::new();
            for (key, value) in f.iter() {
                if parts.len() == 4 {
                    break;
                }
                let rendered = match value {
                    Value::String(v) => Some(clip(v, 40)),
                    Value::Number(v) => Some(v.to_string()),
                    Value::Bool(v) => Some(v.to_string()),
                    _ => None,
                };
                if let Some(v) = rendered {
                    parts.push(format!("{key}={v}"));
                }
            }
            parts.join(" ")
        }
    }
}

/// Clip to `max` chars on a char boundary (… marks the cut).
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let taken: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{taken}…")
}

/// Parse a journal time bound: RFC3339, `YYYY-MM-DDTHH:MM[:SS]`,
/// `YYYY-MM-DD HH:MM[:SS]`, or a bare `YYYY-MM-DD`. Journal times are UTC; a
/// bare date used as `to` extends through the end of that day, inclusive.
fn parse_time_bound(s: &str, end_of_day: bool) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(t);
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(t.and_utc().fixed_offset());
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let t = if end_of_day {
            d.and_hms_opt(23, 59, 59)?
        } else {
            d.and_hms_opt(0, 0, 0)?
        };
        return Some(t.and_utc().fixed_offset());
    }
    None
}

/// Validate step references against the code graph per §2.4.8:
/// - `modify` / `remove` require `found` where the file's capabilities include declarations (`not_found` rejects with candidates)
/// - `create` requires the symbol to be absent on declaration (`not_found` or `unknown`), rejecting `found` on initial start or add
/// - `unknown` passes for all intents
fn validate_plan_refs(
    root: &Path,
    refs: &[crate::plan::StepRef],
    is_create_intent: bool,
) -> Result<(), plan::Rejection> {
    if refs.is_empty() {
        return Ok(());
    }
    let mut store = match crate::agent::graph::SqliteGraphStore::open(root) {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };

    for step_ref in refs {
        let res = match store.resolve_ref(None, Some(&step_ref.path), step_ref.symbol.as_deref()) {
            Ok(r) => r,
            Err(_) => continue,
        };

        match step_ref.intent {
            crate::plan::RefIntent::Modify | crate::plan::RefIntent::Remove => match res {
                crate::agent::graph::ResolveRefResult::NotFound { candidates, .. } => {
                    let hint = if candidates.is_empty() {
                        "verify the file path and symbol name or check resolve_ref".to_string()
                    } else {
                        format!(
                            "candidates: {}",
                            candidates
                                .iter()
                                .map(|c| c.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    };
                    let target = step_ref.symbol.as_deref().unwrap_or(&step_ref.path);
                    return Err(plan::Rejection::new(
                        "ref_not_found",
                        format!("ref '{target}' not found in {}", step_ref.path),
                        hint,
                    ));
                }
                crate::agent::graph::ResolveRefResult::Ambiguous { candidates, .. } => {
                    return Err(plan::Rejection::new(
                        "ref_ambiguous",
                        format!(
                            "ref '{}' in {} is ambiguous ({} candidates)",
                            step_ref.symbol.as_deref().unwrap_or(&step_ref.path),
                            step_ref.path,
                            candidates.len()
                        ),
                        "disambiguate by specifying the scope or kind (e.g. fn::foo)",
                    ));
                }
                crate::agent::graph::ResolveRefResult::Found { .. }
                | crate::agent::graph::ResolveRefResult::Unknown { .. } => {}
            },
            crate::plan::RefIntent::Create => {
                if is_create_intent {
                    if let crate::agent::graph::ResolveRefResult::Found { .. } = res {
                        let target = step_ref.symbol.as_deref().unwrap_or(&step_ref.path);
                        return Err(plan::Rejection::new(
                            "ref_collision",
                            format!(
                                "cannot create ref '{target}': already exists in {}",
                                step_ref.path
                            ),
                            "choose a different symbol name or change intent to modify",
                        ));
                    }
                }
            }
        }
    }
    Ok(())
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
    let limits = plan::Limits {
        max_steps: ctx.plan_limits.max_steps,
    };

    let gate = if matches!(op, plan::Op::Complete) {
        validate_complete(ctx)
    } else {
        validate_evidence(&ctx.root, &op, Some(&ctx.session_id))
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
        } => match plan::open_active_for_session(&ctx.root, Some(&ctx.session_id)) {
            Ok(Some(existing)) => rejection(plan::Rejection {
                code: "plan_exists",
                reason: format!("an active plan already exists: {}", existing.id),
                hint: "use /plan to continue, complete or abandon it first".to_string(),
            }),
            Ok(None) => {
                // Host value, from the model's context and [plan].budget_ratio.
                // It used to be read out of the model's own tool arguments,
                // which let it raise its own ceiling and skip the folding in
                // §2.1.5.
                let budget_limit = ctx
                    .plan_limits
                    .budget_tokens(ctx.context_limit)
                    .max(MIN_PLAN_BUDGET_TOKENS);
                match plan::create(goal, constraints, acceptance, steps, budget_limit, &limits) {
                    Ok(mut created) => {
                        for s in &created.steps {
                            if let Err(rej) = validate_plan_refs(&ctx.root, &s.refs, true) {
                                return rejection(rej);
                            }
                        }
                        created.sessions = vec![ctx.session_id.clone()];
                        let id = created.id.clone();
                        let step_count = created.steps.len();
                        // Journal-first (§2.1.4): the intent carries everything
                        // replay needs to rebuild this plan.
                        let args = serde_json::json!({
                            "goal": created.goal.text,
                            "constraints": created.constraints,
                            "acceptance": created.acceptance.iter().map(|a| a.text.clone()).collect::<Vec<_>>(),
                            "steps": created.steps.iter().map(|s| serde_json::json!({
                                "title": s.title,
                                "kind": s.kind.as_str(),
                                "refs": s.refs,
                            })).collect::<Vec<_>>(),
                            "budget_limit": created.budget.limit,
                            "result_id": created.id,
                            "result_created": created.created,
                            "result_sessions": created.sessions,
                        });
                        match plan::commit(
                            &ctx.root,
                            &ctx.session_id,
                            &mut created,
                            "create",
                            "model",
                            true,
                            args,
                        ) {
                            Ok(_) => {
                                Outcome::ok(format!("plan {id} created with {step_count} steps"))
                            }
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
        plan::Op::Cancel { id, reason } => {
            let active_plans = plan::list_active(&ctx.root);
            match id {
                None => {
                    if active_plans.is_empty() {
                        return Outcome::err("no active plan: create one with op=create first");
                    }
                    if active_plans.len() > 1 {
                        let list = active_plans
                            .iter()
                            .map(|p| format!("{} · {}", p.id, p.goal.text))
                            .collect::<Vec<_>>()
                            .join("\n");
                        return Outcome::err(format!(
                            "multiple active plans found ({}), specify id:\n{list}",
                            active_plans.len()
                        ));
                    }
                    let mut target_plan = active_plans.into_iter().next().unwrap();
                    let pid = target_plan.id.clone();
                    target_plan.status = plan::PlanStatus::Abandoned;
                    target_plan.revision += 1;
                    let args = serde_json::json!({"id": pid});
                    match plan::commit(
                        &ctx.root,
                        &ctx.session_id,
                        &mut target_plan,
                        "cancel",
                        "model",
                        true,
                        args,
                    ) {
                        Ok(_) => Outcome::ok(format!("plan {pid} cancelled")),
                        Err(e) => Outcome::err(format!("plan write failed: {e:#}")),
                    }
                }
                Some(target_id) => {
                    if let Some(mut target_plan) =
                        active_plans.into_iter().find(|p| p.id == target_id)
                    {
                        target_plan.status = plan::PlanStatus::Abandoned;
                        target_plan.revision += 1;
                        let pid = target_plan.id.clone();
                        let args = serde_json::json!({"id": pid});
                        match plan::commit(
                            &ctx.root,
                            &ctx.session_id,
                            &mut target_plan,
                            "cancel",
                            "model",
                            true,
                            args,
                        ) {
                            Ok(_) => return Outcome::ok(format!("plan {pid} cancelled")),
                            Err(e) => return Outcome::err(format!("plan write failed: {e:#}")),
                        }
                    }
                    let mut active =
                        match plan::open_active_for_session(&ctx.root, Some(&ctx.session_id)) {
                            Ok(Some(p)) => p,
                            Ok(None) => {
                                return Outcome::err(
                                    "no active plan: create one with op=create first",
                                );
                            }
                            Err(e) => return Outcome::err(format!("plan store unreadable: {e:#}")),
                        };
                    let op = plan::Op::Cancel {
                        id: Some(target_id.clone()),
                        reason,
                    };
                    match plan::apply(&mut active, op, &limits, ctx.current_step.as_deref()) {
                        Ok(applied) => {
                            if let Err(e) = plan::store(&ctx.root, &active) {
                                return Outcome::err(format!("plan write failed: {e:#}"));
                            }
                            match applied {
                                plan::Applied::Updated { message } => Outcome::ok(message),
                                _ => Outcome::ok(format!("step {target_id} cancelled")),
                            }
                        }
                        Err(r) => rejection(r),
                    }
                }
            }
        }
        other => {
            let mut active = match plan::open_active_for_session(&ctx.root, Some(&ctx.session_id)) {
                Ok(Some(p)) => p,
                Ok(None) => {
                    return Outcome::err(
                        "no active plan: create one with op=create first".to_string(),
                    );
                }
                Err(e) => return Outcome::err(format!("plan store unreadable: {e:#}")),
            };
            // §2.1.4: finishing a step that still carries open assumptions is
            // allowed, but the model has to be told — this is the closure
            // moment the `assumption` note kind never had.
            let starting = match &other {
                plan::Op::Start { id, .. } => Some(id.clone()),
                _ => None,
            };
            let finishing = match &other {
                plan::Op::Finish { id, .. } => Some(id.clone()),
                _ => None,
            };
            // Journal-first (§2.1.4): the intent is recorded ahead of the
            // store, carrying the full op for replay. `show` is read-only
            // and keeps the old plain store with no cursor advance.
            let op_value = serde_json::to_value(&other).unwrap_or(serde_json::Value::Null);
            let op_name = op_value
                .get("op")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown")
                .to_string();
            let readonly_show = op_name == "show";
            if let plan::Op::Start { ref id, .. } = other {
                if let Some(step) = active.step(id) {
                    let is_initial_start = step.status == plan::StepStatus::Pending;
                    if let Err(rej) = validate_plan_refs(&ctx.root, &step.refs, is_initial_start) {
                        return rejection(rej);
                    }
                }
            }
            if let plan::Op::Add { ref refs, .. } = other {
                if let Err(rej) = validate_plan_refs(&ctx.root, refs, true) {
                    return rejection(rej);
                }
            }
            match plan::apply(&mut active, other, &limits, ctx.current_step.as_deref()) {
                Ok(applied) => {
                    if readonly_show {
                        if let Err(e) = plan::store(&ctx.root, &active) {
                            return Outcome::err(format!("plan write failed: {e:#}"));
                        }
                    } else if let Err(e) = plan::commit(
                        &ctx.root,
                        &ctx.session_id,
                        &mut active,
                        &op_name,
                        "model",
                        true,
                        op_value,
                    ) {
                        return Outcome::err(format!("plan write failed: {e:#}"));
                    }
                    if let Some(ref id) = starting
                        && let Ok(Some(sha)) = crate::agent::checkpoints::snapshot_boundary(
                            &ctx.root,
                            ctx.shadow_store,
                            &ctx.session_id,
                            &format!("step_{id}_start"),
                        )
                    {
                        ctx.journal.push((sha, format!("step_{id}_start")));
                    }
                    if let Some(ref id) = finishing
                        && let Ok(Some(sha)) = crate::agent::checkpoints::snapshot_boundary(
                            &ctx.root,
                            ctx.shadow_store,
                            &ctx.session_id,
                            &format!("step_{id}_finish"),
                        )
                    {
                        ctx.journal.push((sha, format!("step_{id}_finish")));
                    }
                    match applied {
                        plan::Applied::Created(_) => Outcome::ok("plan created".to_string()),
                        plan::Applied::Updated { message } => {
                            let msg = with_assumption_warning(ctx, finishing.as_deref(), message);
                            let msg =
                                with_evidence_ts_warning(ctx, finishing.as_deref(), &active, msg);
                            let msg = with_misattribution_warning(
                                ctx,
                                finishing.as_deref(),
                                &active,
                                msg,
                            );
                            Outcome::ok(msg)
                        }
                        plan::Applied::Shown { text } => Outcome::ok(text),
                        plan::Applied::Completed => {
                            Outcome::ok(format!("plan {} completed", active.id))
                        }
                    }
                }
                Err(r) => {
                    // the rejection counter is plan state, so persist it too —
                    // behind a rejection intent, keeping the cursor discipline
                    let _ = plan::commit(
                        &ctx.root,
                        &ctx.session_id,
                        &mut active,
                        &op_name,
                        "model",
                        false,
                        op_value,
                    );
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
    let mut active = match plan::open_active_for_session(&ctx.root, Some(&ctx.session_id)) {
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

    let (evidence, receipt) = match item.kind() {
        plan::AcceptanceKind::Manual(_) => (Vec::new(), None),
        plan::AcceptanceKind::Command(command) => {
            let command = command.to_string();
            // The acceptance text arrives from the model on `plan create`, so
            // it is model-controlled input that the host is about to execute.
            // It goes through the same classifier as `bash`, and anything that
            // would need approval is refused rather than silently run: an
            // acceptance criterion is not the place to ask.
            match safety::classify(&command) {
                safety::Verdict::Blocked(reason) => {
                    return rejection(plan::Rejection {
                        code: "protected_path",
                        reason: format!(
                            "acceptance {index} touches protected path ({reason}): {command}"
                        ),
                        hint: "acceptance commands must not touch host-owned state".to_string(),
                    });
                }
                safety::Verdict::NeedsApproval(reason) => {
                    return rejection(plan::Rejection {
                        code: "unsafe_acceptance",
                        reason: format!("acceptance {index} would run a {reason} command: {command}"),
                        hint: "acceptance commands run without asking, so they must be safe;                            rewrite it or have the user waive the item"
                            .to_string(),
                    });
                }
                safety::Verdict::Safe => {}
            }
            // interval consistency (§2.1.4): digest the traversed state
            // before AND after the run. A mutation mid-check (background
            // job, concurrent subagent) means the check proved nothing —
            // no receipt is issued and the item stays unverified.
            let paths = plan::digest_paths(&active);
            let state_before = plan::state_digest(&ctx.root, &paths, &command);
            let started_at = plan::now();
            let run = exec::bash(ctx, &command, Some(ACCEPTANCE_TIMEOUT_SECS), false);
            let finished_at = plan::now();
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
            let state_after = plan::state_digest(&ctx.root, &paths, &command);
            if state_before != state_after {
                return rejection(plan::Rejection {
                    code: "state_changed_during_check",
                    reason: format!(
                        "acceptance {index} ran while tracked state moved; no receipt issued"
                    ),
                    hint: "run verify again on the settled state".to_string(),
                });
            }
            let output_hash = blake3::hash(run.output.as_bytes()).to_hex().to_string();
            // exit travels with the outcome now: receipts only issue on
            // `ok` runs, so this is zero in practice, but sourced rather
            // than assumed — a nonzero code with ok would be a loud bug,
            // not a silent receipt
            let exit_code = run.exit_code;
            let receipt_fields = serde_json::json!({
                "check_definition_hash": plan::check_definition_hash(&command),
                "runner": "exec",
                "command": command.clone(),
                "args": serde_json::Value::Null,
                "cwd": ctx.root.display().to_string(),
                "started_at": started_at,
                "finished_at": finished_at,
                "state_before": state_before,
                "state_after": state_after,
                "exit": exit_code,
                "output_hash": output_hash,
                "paths": paths,
            });
            let seq = match crate::agent::journal::Journal::open(&ctx.root, &ctx.session_id) {
                Ok(mut journal) => {
                    match journal.append_verification_receipt(index, receipt_fields) {
                        Ok(seq) => seq,
                        Err(e) => {
                            return Outcome::err(format!("receipt journal unwritable: {e:#}"));
                        }
                    }
                }
                Err(e) => {
                    return Outcome::err(format!("receipt journal unwritable: {e:#}"));
                }
            };
            // receipts issue only on `ok` runs (failures return above),
            // so the sourced code below is zero — kept as data, not dogma
            let receipt = plan::Receipt {
                session: ctx.session_id.clone(),
                seq,
                state_digest: state_after.clone(),
                command: Some(command.clone()),
                exit: exit_code,
                at: finished_at.clone(),
                check_definition_hash: Some(plan::check_definition_hash(&command)),
                runner: Some("exec".to_string()),
                args: None,
                cwd: Some(ctx.root.display().to_string()),
                started_at: Some(started_at),
                finished_at: Some(finished_at),
                state_before: Some(state_before),
                state_after: Some(state_after),
                output_hash: Some(output_hash),
                paths,
            };
            (Vec::new(), Some(receipt))
        }
        plan::AcceptanceKind::Text(_) => {
            let Some((step_id, evidence)) = unspent_verify_evidence(&ctx.root, &active, index)
            else {
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
            // attachment receipt (§2.1.4): the evidence records are
            // immutable, but the world they describe is not. Pin a
            // point-in-time digest over the traversed inputs so a later
            // file move stales this item through the same machinery as
            // command checks — instead of letting verified evidence
            // silently outlive the state it attested.
            let paths = plan::digest_paths(&active);
            let digest = plan::state_digest(&ctx.root, &paths, "");
            let at = plan::now();
            let receipt_fields = serde_json::json!({
                "runner": "evidence",
                "step_id": step_id,
                "evidence_refs": evidence.clone(),
                "cwd": ctx.root.display().to_string(),
                "finished_at": at,
                "state_before": digest,
                "state_after": digest,
                "state_digest": digest,
                "paths": paths,
            });
            let seq = match crate::agent::journal::Journal::open(&ctx.root, &ctx.session_id) {
                Ok(mut journal) => {
                    match journal.append_verification_receipt(index, receipt_fields) {
                        Ok(seq) => seq,
                        Err(e) => {
                            return Outcome::err(format!("receipt journal unwritable: {e:#}"));
                        }
                    }
                }
                Err(e) => {
                    return Outcome::err(format!("receipt journal unwritable: {e:#}"));
                }
            };
            let receipt = plan::Receipt {
                session: ctx.session_id.clone(),
                seq,
                state_digest: digest.clone(),
                command: None,
                exit: None,
                at: at.clone(),
                check_definition_hash: None,
                runner: Some("evidence".to_string()),
                args: None,
                cwd: Some(ctx.root.display().to_string()),
                started_at: None,
                finished_at: Some(at),
                state_before: Some(digest.clone()),
                state_after: Some(digest),
                output_hash: None,
                paths,
            };
            (evidence, Some(receipt))
        }
    };

    match plan::verify_acceptance(&mut active, index, evidence, supplied, receipt) {
        Ok(applied) => {
            // the receipt rides the commit args so replay restores
            // validation without re-running the check
            let receipt_value = active
                .acceptance
                .get(index)
                .and_then(|item| item.validation.receipts.last())
                .and_then(|r| serde_json::to_value(r).ok());
            let mut args = serde_json::json!({
                "acceptance": index,
                "evidence_refs": active.acceptance.get(index).map(|item| item.evidence.clone()).unwrap_or_default(),
            });
            if let Some(receipt_value) = receipt_value {
                args["receipt"] = receipt_value;
            }
            if let Err(e) = plan::commit(
                &ctx.root,
                &ctx.session_id,
                &mut active,
                "verify",
                "model",
                true,
                args,
            ) {
                return Outcome::err(format!("plan write failed: {e:#}"));
            }
            match applied {
                plan::Applied::Updated { message } => Outcome::ok(message),
                _ => Outcome::ok(format!("acceptance {index} verified")),
            }
        }
        Err(r) => {
            let args = serde_json::json!({"acceptance": index});
            let _ = plan::commit(
                &ctx.root,
                &ctx.session_id,
                &mut active,
                "verify",
                "model",
                false,
                args,
            );
            rejection(r)
        }
    }
}

/// Append the open-assumption warning to a successful `finish` (§2.1.4).
///
/// Non-blocking on purpose: the step is already finished when this runs. The
/// point is that an assumption cannot quietly outlive the step that made it —
/// the model either resolves it with `note { resolves }` or carries it
/// forward knowingly.
fn with_assumption_warning(ctx: &ToolCtx, finished_step: Option<&str>, message: String) -> String {
    let Some(step) = finished_step else {
        return message;
    };
    let open =
        crate::agent::journal::Journal::open_assumptions(&ctx.root, Some(step)).unwrap_or_default();
    if open.is_empty() {
        return message;
    }
    let list = open
        .iter()
        .map(|item| item.label(80))
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "{message}\nwarning: step {step} has {} open assumption(s) ({list}) — resolve with \
         note {{ kind: \"assumption\", resolves: <seq> }} or convert before completing",
        open.len()
    )
}

fn with_evidence_ts_warning(
    ctx: &ToolCtx,
    finished_step: Option<&str>,
    active: &plan::Plan,
    message: String,
) -> String {
    let Some(step) = finished_step else {
        return message;
    };
    let Some(s) = active.step(step) else {
        return message;
    };
    let warns = crate::agent::journal::Journal::stale_evidence_warnings(
        &ctx.root,
        &active.id,
        step,
        &s.evidence,
    );
    if warns.is_empty() {
        return message;
    }
    format!("{message}\nwarning: {}", warns.join("; "))
}

fn with_misattribution_warning(
    ctx: &ToolCtx,
    finished_step: Option<&str>,
    active: &plan::Plan,
    message: String,
) -> String {
    let Some(step) = finished_step else {
        return message;
    };
    let warns =
        crate::agent::journal::Journal::step_misattribution_warnings(&ctx.root, active, step);
    if warns.is_empty() {
        return message;
    }
    format!("{message}\nwarning: {}", warns.join("; "))
}

/// A verify step whose evidence no acceptance item has spent yet, with that
/// evidence. `None` when every verify step's records are already accounted
/// for — which is the case this whole function exists to catch.
/// Records from a stale step epoch (subagent work predating a reopen) are
/// excluded: they belong to the undone attempt, not the current one (§2.2.4).
fn unspent_verify_evidence(
    root: &Path,
    active: &plan::Plan,
    index: usize,
) -> Option<(String, Vec<plan::EvidenceRef>)> {
    let spent: Vec<&plan::EvidenceRef> = active
        .acceptance
        .iter()
        .enumerate()
        .filter(|(other, item)| *other != index && item.status == plan::AcceptanceStatus::Passed)
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
                .filter(|reference| evidence_epoch_current(root, &active.id, step, reference))
                .cloned()
                .collect();
            (!fresh.is_empty()).then(|| (step.id.clone(), fresh))
        })
}

/// True unless the referenced record positively belongs to an older step
/// epoch. Unresolvable references fail open here — resolution problems
/// surface with their own error at validation.
fn evidence_epoch_current(
    root: &Path,
    plan_id: &str,
    step: &plan::Step,
    reference: &plan::EvidenceRef,
) -> bool {
    let record = match crate::agent::journal::Journal::evidence(
        root,
        plan_id,
        Some(&step.id),
        reference,
        None,
    ) {
        Ok(record) => record,
        Err(_) => return true,
    };
    let Some(record) = record else {
        return true;
    };
    crate::agent::journal::epoch_matches(&record, step.step_epoch)
}

/// The gate on `plan finish`: the step must have host-recorded evidence of the
/// right kind since it started.
///
/// `verify` is not handled here — `verify_acceptance` settles an acceptance
/// item on its own terms, per item, and this function used to short-circuit
/// that with a plan-wide "is there any verify evidence anywhere" check.
fn validate_evidence(root: &Path, op: &plan::Op, session_id: Option<&str>) -> Result<(), String> {
    let plan::Op::Finish { id, .. } = op else {
        return Ok(());
    };
    let status = plan::open_active_for_session(root, session_id)
        .ok()
        .flatten()
        .and_then(|p| p.step(id).map(|s| s.status));
    if status != Some(plan::StepStatus::InProgress) {
        // `finish` on a step that is not in progress is rejected by the
        // validator with a clearer reason than a missing-evidence error.
        return Ok(());
    }
    let active = plan::open_active_for_session(root, session_id)
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
    let active = plan::open_active_for_session(&root, Some(&ctx.session_id))
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
        if acceptance.status != plan::AcceptanceStatus::Passed {
            continue;
        }
        // state moved under a recorded check after verification: the pure
        // complete gate rejects this too, but failing here names the fix
        // (re-verify) before the op is even attempted
        if acceptance.validation.status == plan::ValidationStatus::Stale {
            return Err(format!(
                "acceptance_stale: acceptance {index} went stale after verification (tracked files changed); re-verify it, then complete"
            ));
        }
        match acceptance.kind() {
            plan::AcceptanceKind::Command(command) => {
                match safety::classify(command) {
                    safety::Verdict::Blocked(reason) => {
                        return Err(format!(
                            "protected_path: acceptance {index} touches protected path ({reason}) at completion: {command}"
                        ));
                    }
                    safety::Verdict::NeedsApproval(reason) => {
                        return Err(format!(
                            "unsafe_acceptance: acceptance {index} would run a {reason} command                          at completion: {command}"
                        ));
                    }
                    safety::Verdict::Safe => {}
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
                // Passed rather than waived: it should not have been possible
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
    let mut stale_epoch = 0usize;
    // Epoch of the step being validated. Acceptance-level checks have no
    // step; their refs were epoch-filtered when selected (unspent evidence).
    let step_epoch = if step_id == "acceptance" {
        None
    } else {
        plan::read_plan_file(root, plan_id).and_then(|plan| {
            plan.steps
                .iter()
                .find(|step| step.id == step_id)
                .map(|step| step.step_epoch)
        })
    };
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
        // Records from a stale step epoch (subagent work predating a reopen)
        // belong to the undone attempt, not the current one (§2.2.4).
        if let Some(epoch) = step_epoch
            && !crate::agent::journal::epoch_matches(&record, epoch)
        {
            stale_epoch += 1;
            continue;
        }
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
        if stale_epoch > 0 {
            return Err(format!(
                "stale_epoch: all {stale_epoch} evidence record(s) predate the last reopen of step {step_id}; do the work again under the current epoch"
            ));
        }
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
        let think = execute(&mut ctx, "think", &json!({"thought": "step one: read"}));
        assert!(think.ok, "{}", think.output);
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
                "acceptance": [],
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
                &json!({"op": "create", "goal": "tiny context", "acceptance": [],
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
            &json!({"op": "create", "goal": "over the limit", "acceptance": [], "steps": steps}),
        );
        assert!(!refused.ok, "{}", refused.output);
        assert!(
            refused.output.contains("too_many_steps") || refused.output.contains("2"),
            "{}",
            refused.output
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// PLAN mode refuses `git_branch create|switch` at dispatch already; the
    /// schema should not invite the model to spend a turn discovering that.
    #[test]
    fn plan_mode_advertises_git_branch_without_its_mutating_actions() {
        let actions = |plan_mode: bool| {
            tool_specs(plan_mode)
                .into_iter()
                .find(|spec| spec.name == "git_branch")
                .expect("git_branch stays available for inspection")
                .parameters["properties"]["action"]["enum"]
                .clone()
        };
        assert_eq!(actions(true), json!(["list", "current"]));
        assert_eq!(
            actions(false),
            json!(["list", "current", "create", "switch"])
        );
    }

    /// #14: a label must never advertise what the gate would refuse. The
    /// dispatcher decides by action (`is_mutating_call`), so a `ReadOnly`
    /// label on a mixed tool cannot execute — but the PLAN-mode schema is
    /// built from the label, and a tool that outgrows it would silently
    /// offer mutations. This walks every advertised `(tool, action)` pair
    /// and requires the advertised surface to agree with the gate.
    /// Tools without an `action` enum have nothing to cross-check.
    #[test]
    fn plan_mode_never_advertises_a_mutating_action() {
        for plan_mode in [false, true] {
            for spec in tool_specs(plan_mode) {
                let actions: Vec<String> = spec
                    .parameters
                    .pointer("/properties/action/enum")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                for action in actions {
                    let args = json!({"action": action});
                    assert!(
                        !plan_mode || !is_mutating_call(&spec.name, &args),
                        "PLAN mode advertises {} with mutating action {action:?}",
                        spec.name
                    );
                }
            }
        }
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

    /// §2.1.4 lets a verify step close on "diagnostics with zero errors". The
    /// record was defined in §2.2.2 and never written, so that branch was
    /// unreachable — this is the evidence path, now that it exists.
    #[test]
    fn clean_diagnostics_close_a_verify_step() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({"op": "create", "goal": "diagnostics as evidence", "acceptance": [],
                    "steps": [{"title": "check", "kind": "verify"}]}),
        );
        assert!(created.ok, "{}", created.output);
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);

        let mut journal = crate::agent::journal::Journal::open(&dir, "diagnostics").unwrap();
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

    /// A subagent mutating after its step was reopened would attach stale
    /// work to a fresh epoch (§2.2.4). The dispatcher refuses the mutation
    /// instead; read-only tools keep working, and a fresh spawn proceeds.
    #[test]
    fn subagent_mutation_refused_after_step_reopen() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({"op": "create", "goal": "guarded work", "acceptance": [],
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

    /// Evidence stamped with a pre-reopen epoch belongs to the undone
    /// attempt and must not validate the reworked step (§2.2.4). Unstamped
    /// records (main agent, legacy) always count.
    #[test]
    fn stale_epoch_evidence_rejected_at_validation() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({"op": "create", "goal": "epoch filter", "acceptance": [],
                    "steps": [{"title": "change things"}]}),
        );
        assert!(created.ok, "{}", created.output);
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;

        let mut journal = crate::agent::journal::Journal::open(&dir, "epoch-test").unwrap();
        journal.set_attribution(Some("1".into()), Some(plan_id.clone()), "main");
        journal.set_epoch(Some(0));
        let stale_seq = journal
            .append_evidence("file_diff", json!({"path": "src/main.rs"}))
            .unwrap();
        journal.set_epoch(None);
        let fresh_seq = journal
            .append_evidence("file_diff", json!({"path": "src/main.rs"}))
            .unwrap();

        // The step moved to epoch 1 (reopen semantics without the status dance).
        let mut active = plan::open_active(&dir).unwrap().unwrap();
        active.steps[0].step_epoch = 1;
        plan::store(&dir, &active).unwrap();

        let stale = vec![plan::EvidenceRef {
            session: "epoch-test".into(),
            seq: stale_seq,
        }];
        let err = validate_attached_records(&dir, &plan_id, "1", plan::StepKind::Change, &stale)
            .unwrap_err();
        assert!(err.contains("stale_epoch"), "{err}");

        let fresh = vec![plan::EvidenceRef {
            session: "epoch-test".into(),
            seq: fresh_seq,
        }];
        validate_attached_records(&dir, &plan_id, "1", plan::StepKind::Change, &fresh)
            .expect("unstamped evidence counts");
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
        assert!(
            !finish.output.contains("warning:"),
            "finish output should not contain warning: {}",
            finish.output
        );

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
    fn finish_warns_when_evidence_predates_step_start() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "boundary test",
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
                "goal": "close the assumption loop",
                "steps": [{"title": "make the change", "kind": "change"}],
            }),
        );
        assert!(created.ok, "{}", created.output);
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);

        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        let mut journal = crate::agent::journal::Journal::open(&dir, "assumption").unwrap();
        journal.set_attribution(Some("1".into()), Some(plan_id), "main");
        let seq = journal
            .append(
                "note",
                json!({"by": "model", "note": "assumption", "text": "the config key is stable"}),
            )
            .unwrap();
        // evidence for the change step, so `finish` is not rejected for that
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
            plan::AcceptanceStatus::Passed
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

    /// A text acceptance verified on step evidence pins an attachment
    /// receipt, so a later move of the traversed files stales it through
    /// the same machinery as command checks — instead of letting verified
    /// evidence silently outlive the state it attested.
    #[test]
    fn text_verify_pins_attachment_receipt_and_later_diff_stales_it() {
        let (mut ctx, dir) = proj();
        fs::write(dir.join("tracked.rs"), "one").unwrap();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "evidence with a receipt",
                "acceptance": ["the code is formatted"],
                "steps": [{"title": "verify", "kind": "verify", "refs": ["tracked.rs"]}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        let mut journal = crate::agent::journal::Journal::open(&dir, "verify-rules").unwrap();
        journal.set_attribution(Some("1".into()), Some(plan_id), "main");
        journal
            .append_evidence("tool_result", json!({"tool": "bash", "ok": true}))
            .unwrap();

        let verified = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(verified.ok, "{}", verified.output);
        let plan = plan::open_active(&dir).unwrap().unwrap();
        let item = &plan.acceptance[0];
        assert_eq!(item.validation.status, plan::ValidationStatus::Passed);
        assert_eq!(item.validation.receipts.len(), 1);
        let receipt = &item.validation.receipts[0];
        assert_eq!(receipt.runner.as_deref(), Some("evidence"));
        assert_eq!(receipt.state_before, receipt.state_after);
        assert!(receipt.paths.iter().any(|p| p == "tracked.rs"));

        // close the step so `complete` reaches the acceptance gate; a
        // later move of the traversed file must then refuse completion
        // until the item is re-verified
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        assert!(
            plan_op(
                &mut ctx,
                &json!({"op": "cancel", "id": "1", "reason": "done here"})
            )
            .ok
        );
        let sid = ctx.session_id.clone();
        assert!(plan::invalidate_on_diff(&dir, &sid, &["tracked.rs".to_string()]).unwrap());
        let completed = plan_op(&mut ctx, &json!({"op": "complete"}));
        assert!(!completed.ok, "{}", completed.output);
        assert!(
            completed.output.contains("acceptance_stale"),
            "{}",
            completed.output
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

    /// Existence probe the acceptance runner can execute. `test -f` is POSIX
    /// shell syntax, and this suite also runs on Windows, where the shell
    /// has no `test` builtin — so each platform spells the probe its own
    /// way, and the test exercises the re-run at `complete`, not the shell.
    #[cfg(unix)]
    fn gate_probe_command(flag: &std::path::Path) -> String {
        format!("test -f {}", flag.display())
    }

    /// Windows spelling of [`gate_probe_command`]: `Get-Item` on a missing
    /// path exits 1, on a present one 0. Deliberately no double quotes,
    /// parentheses or semicolons anywhere: the command travels through Rust
    /// argv quoting and `cmd /C` parsing before it reaches PowerShell, and
    /// any of those would be mangled on the way (verified by watching a
    /// parenthesised form exit 0 either way). Single quotes pass through
    /// `cmd` literally, so paths with spaces survive.
    #[cfg(windows)]
    fn gate_probe_command(flag: &std::path::Path) -> String {
        format!(
            "powershell -NoProfile -Command Get-Item '{}'",
            flag.display()
        )
    }

    /// `complete` runs `cmd:` items again instead of trusting the verify that
    /// happened earlier: a criterion that stopped passing must block
    /// completion (§2.1.2).
    #[test]
    fn complete_reruns_cmd_acceptance_and_refuses_when_it_now_fails() {
        let (mut ctx, dir) = proj();
        let flag = dir.join("gate.txt");
        fs::write(&flag, "ok").unwrap();
        let command = gate_probe_command(&flag);
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

    /// `plan verify` on a `cmd:` item records an interval receipt: equal
    /// before/after digests, exec runner, and a matching journal record.
    #[test]
    fn verify_cmd_issues_interval_receipt() {
        let (mut ctx, dir) = proj();
        let flag = dir.join("gate.txt");
        fs::write(&flag, "ok").unwrap();
        let command = gate_probe_command(&flag);
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "receipts",
                "acceptance": [format!("cmd: {command}")],
                "steps": [{"title": "verify", "kind": "verify", "refs": ["gate.txt"]}]
            }),
        );
        assert!(created.ok, "{}", created.output);

        let verified = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(verified.ok, "{}", verified.output);

        let plan = plan::open_active(&dir).unwrap().unwrap();
        let item = &plan.acceptance[0];
        assert_eq!(item.status, plan::AcceptanceStatus::Passed);
        assert_eq!(item.validation.status, plan::ValidationStatus::Passed);
        assert_eq!(item.validation.receipts.len(), 1);
        let receipt = &item.validation.receipts[0];
        assert_eq!(receipt.runner.as_deref(), Some("exec"));
        assert_eq!(receipt.state_before, receipt.state_after);
        assert_eq!(receipt.exit, Some(0));
        assert!(receipt.paths.iter().any(|p| p == "gate.txt"));
        let records = crate::agent::journal::Journal::records(&dir).unwrap();
        assert!(
            records.iter().any(|r| {
                r.kind == "verification_receipt"
                    && r.fields.get("acceptance_id").and_then(|v| v.as_u64()) == Some(0)
                    && r.fields.get("state_before") == r.fields.get("state_after")
            }),
            "verification_receipt journal record missing"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// A check that races a mutation proves nothing: when the command
    /// itself moves tracked state mid-run, verify is rejected and no
    /// receipt is issued.
    #[test]
    fn verify_cmd_rejects_when_state_moves_mid_run() {
        let (mut ctx, dir) = proj();
        let flag = dir.join("gate.txt");
        fs::write(&flag, "ok").unwrap();
        // relative path: the command runs with the project root as cwd,
        // and quoting a temp path would only muddy the classifier
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "races",
                "acceptance": ["cmd: echo hi >> gate.txt"],
                "steps": [{"title": "verify", "kind": "verify", "refs": ["gate.txt"]}]
            }),
        );
        assert!(created.ok, "{}", created.output);

        let verified = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!verified.ok, "{}", verified.output);
        assert!(
            verified.output.contains("state_changed_during_check"),
            "{}",
            verified.output
        );
        let plan = plan::open_active(&dir).unwrap().unwrap();
        assert_eq!(
            plan.acceptance[0].validation.status,
            plan::ValidationStatus::Pending
        );
        assert!(plan.acceptance[0].validation.receipts.is_empty());
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
        assert!(
            complete.output.contains("Pending acceptance items without cmd: prefix require user waiver (/plan waive <index>) or conversion to verify steps."),
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
    fn ast_grep_matches_by_shape_with_metavariables() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("demo.rs"),
            "fn main() {\n    let a = Ok(42);\n    let b = Err(\"x\");\n    let c = Ok(Some(7));\n}\n",
        )
        .unwrap();
        let mut ctx = ToolCtx::new(dir.path());

        // single metavariable: both Ok(...) calls, not the Err
        let o = execute(&mut ctx, "ast_grep", &json!({"pattern": "Ok($E)"}));
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("demo.rs:2"), "{}", o.output);
        assert!(o.output.contains("demo.rs:4"), "{}", o.output);
        assert!(!o.output.contains("Err"), "{}", o.output);
        // bindings are reported
        assert!(o.output.contains("$E"), "{}", o.output);

        // structural: the second argument must be there, so no match
        let o = execute(&mut ctx, "ast_grep", &json!({"pattern": "Ok($A, $B)"}));
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("0 matches"), "{}", o.output);

        // multi metavariable matches any argument list, only for the right fn
        fs::write(
            dir.path().join("calls.rs"),
            "fn run() {\n    f(1);\n    f(1, 2);\n    g(3);\n}\n",
        )
        .unwrap();
        let o = execute(&mut ctx, "ast_grep", &json!({"pattern": "f($$$ARGS)"}));
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("calls.rs:2"), "{}", o.output);
        assert!(o.output.contains("calls.rs:3"), "{}", o.output);
        assert!(!o.output.contains("g(3)"), "{}", o.output);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ast_grep_ignores_comments_and_filters_by_language() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("c.rs"),
            "fn f() {\n    let x = Ok( /* why */ 42);\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("p.py"),
            "print(\"a\")\nprint(\"a\", \"b\")\n",
        )
        .unwrap();
        let mut ctx = ToolCtx::new(dir.path());

        // comments do not break a match
        let o = execute(&mut ctx, "ast_grep", &json!({"pattern": "Ok($E)"}));
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("c.rs:2"), "{}", o.output);

        // language inferred per file: the python pattern only sees the .py
        let o = execute(&mut ctx, "ast_grep", &json!({"pattern": "print($X)"}));
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("p.py:1"), "{}", o.output);
        assert!(!o.output.contains("p.py:2"), "{}", o.output);

        // an explicit lang restricts a directory scan
        let o = execute(
            &mut ctx,
            "ast_grep",
            &json!({"pattern": "print($X)", "lang": "rust"}),
        );
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("0 matches"), "{}", o.output);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ast_grep_rejects_bad_patterns_and_escapes() {
        let (mut ctx, dir) = proj();
        // unparseable pattern
        let o = execute(&mut ctx, "ast_grep", &json!({"pattern": "Ok("}));
        assert!(!o.ok, "{}", o.output);
        assert!(o.output.contains("does not parse"), "{}", o.output);
        // lowercase $name is not a metavariable: it cannot parse in Rust
        let o = execute(&mut ctx, "ast_grep", &json!({"pattern": "Ok($x)"}));
        assert!(!o.ok, "{}", o.output);
        // path escapes are rejected like every other tool
        let o = execute(
            &mut ctx,
            "ast_grep",
            &json!({"pattern": "Ok($E)", "path": "../outside"}),
        );
        assert!(!o.ok, "{}", o.output);
        // unknown lang
        let o = execute(
            &mut ctx,
            "ast_grep",
            &json!({"pattern": "Ok($E)", "lang": "cobol"}),
        );
        assert!(!o.ok, "{}", o.output);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ast_grep_supports_c_cpp_csharp_and_java() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("main.c"),
            "int calculate(int x) { return x * 2; }\nint main() { return calculate(5); }\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("service.cpp"),
            "class Engine { void start() {} };\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("App.cs"),
            "class Greeter { void SayHello() {} }\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("Hello.java"),
            "class Hello { void greet() {} }\n",
        )
        .unwrap();
        let mut ctx = ToolCtx::new(dir.path());

        let c_res = execute(&mut ctx, "ast_grep", &json!({"pattern": "calculate($X)"}));
        assert!(c_res.ok, "{}", c_res.output);
        assert!(c_res.output.contains("main.c:2"), "{}", c_res.output);

        let cpp_res = execute(&mut ctx, "ast_grep", &json!({"pattern": "void start() {}"}));
        assert!(cpp_res.ok, "{}", cpp_res.output);
        assert!(
            cpp_res.output.contains("service.cpp:1"),
            "{}",
            cpp_res.output
        );

        let cs_res = execute(
            &mut ctx,
            "ast_grep",
            &json!({"pattern": "void SayHello() {}"}),
        );
        assert!(cs_res.ok, "{}", cs_res.output);
        assert!(cs_res.output.contains("App.cs:1"), "{}", cs_res.output);

        let java_res = execute(&mut ctx, "ast_grep", &json!({"pattern": "void greet() {}"}));
        assert!(java_res.ok, "{}", java_res.output);
        assert!(
            java_res.output.contains("Hello.java:1"),
            "{}",
            java_res.output
        );

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
    fn plan_cancel_auto_selects_single_active_plan() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "single plan cancel test",
                "steps": [{"title": "step 1"}]
            }),
        );
        assert!(created.ok, "{}", created.output);

        // Cancel with op: "cancel" and missing id when 1 active plan exists
        let cancelled = plan_op(&mut ctx, &json!({"op": "cancel"}));
        assert!(cancelled.ok, "{}", cancelled.output);
        assert!(
            cancelled.output.contains("cancelled"),
            "{}",
            cancelled.output
        );

        // The active plan is now abandoned
        assert!(plan::open_active(&dir).unwrap().is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn plan_cancel_demands_id_when_multiple_active_plans() {
        let (mut ctx, dir) = proj();
        // Create first active plan
        let p1 = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "first plan",
                "steps": [{"title": "step 1"}]
            }),
        );
        assert!(p1.ok);

        // Manually create a second active plan on disk
        let mut second = plan::open_active(&dir).unwrap().unwrap();
        second.id = plan::new_id();
        second.goal.text = "second plan".to_string();
        plan::store(&dir, &second).unwrap();

        assert_eq!(plan::list_active(&dir).len(), 2);

        // Cancel without id should fail and list active plans
        let cancelled = plan_op(&mut ctx, &json!({"op": "cancel"}));
        assert!(!cancelled.ok);
        assert!(
            cancelled.output.contains("multiple active plans"),
            "{}",
            cancelled.output
        );
        assert!(
            cancelled.output.contains("first plan"),
            "{}",
            cancelled.output
        );
        assert!(
            cancelled.output.contains("second plan"),
            "{}",
            cancelled.output
        );

        // Cancel with explicit id succeeds
        let cancel_second = plan_op(&mut ctx, &json!({"op": "cancel", "id": second.id}));
        assert!(cancel_second.ok, "{}", cancel_second.output);
        assert!(cancel_second.output.contains(&second.id));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn step_diff_tool_flow() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "step diff test",
                "steps": [
                    {"title": "first step", "kind": "change"},
                    {"title": "second step", "kind": "change"}
                ]
            }),
        );
        assert!(created.ok);

        // 1. step_diff requires step_id
        let no_id = execute(&mut ctx, "step_diff", &json!({}));
        assert!(!no_id.ok);
        assert!(no_id.output.contains("step_id is required"));

        // 2. Pending step reports it has not started
        let pending = execute(&mut ctx, "step_diff", &json!({"step_id": "1"}));
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
        let diff_prog = execute(&mut ctx, "step_diff", &json!({"step_id": "1"}));
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
        let diff1 = execute(&mut ctx, "step_diff", &json!({"step_id": "1"}));
        assert!(diff1.ok, "{}", diff1.output);
        assert!(diff1.output.contains("feature1.rs"));
        assert!(diff1.output.contains("feat1"));
        assert!(!diff1.output.contains("feature2.rs"));

        // 6. Querying step 2 diff shows only step 2 changes
        let diff2 = execute(&mut ctx, "step_diff", &json!({"step_id": "2"}));
        assert!(diff2.ok, "{}", diff2.output);
        assert!(diff2.output.contains("feature2.rs"));
        assert!(diff2.output.contains("feat2"));
        assert!(!diff2.output.contains("feat1"));

        // 7. Path-scoped step diff
        let diff_path = execute(
            &mut ctx,
            "step_diff",
            &json!({"step_id": "2", "path": "src/feature2.rs"}),
        );
        assert!(diff_path.ok, "{}", diff_path.output);
        assert!(diff_path.output.contains("feature2.rs"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_ref_tool_execution() {
        let (mut ctx, dir) = proj();
        fs::write(
            dir.join("src/calc.rs"),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
        )
        .unwrap();

        let mut store = crate::agent::graph::SqliteGraphStore::open(&dir).unwrap();
        crate::agent::graph_index::index_project(&mut store, &dir).unwrap();

        let outcome = execute(
            &mut ctx,
            "resolve_ref",
            &json!({"path": "src/calc.rs", "symbol": "add"}),
        );
        assert!(outcome.ok, "{}", outcome.output);
        assert!(outcome.output.contains("pub fn add"));
        assert!(outcome.output.contains("\"status\": \"found\""));

        let not_found = execute(
            &mut ctx,
            "resolve_ref",
            &json!({"ref": "src/calc.rs::subtract"}),
        );
        assert!(not_found.ok, "{}", not_found.output);
        assert!(not_found.output.contains("\"status\": \"not_found\""));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn plan_refs_validation_enforces_intent() {
        let (mut ctx, dir) = proj();
        fs::write(
            dir.join("src/calc.rs"),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
        )
        .unwrap();
        fs::write(dir.join("notes.txt"), "plain notes\n").unwrap();

        let mut store = crate::agent::graph::SqliteGraphStore::open(&dir).unwrap();
        crate::agent::graph_index::index_project(&mut store, &dir).unwrap();

        // 1. Create plan with modify intent on missing symbol -> rejected!
        let rej = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "test goal",
                "steps": [{
                    "title": "step 1",
                    "kind": "change",
                    "refs": [{"path": "src/calc.rs", "symbol": "nonexistent", "intent": "modify"}]
                }]
            }),
        );
        assert!(!rej.ok, "should reject missing ref on modify");
        assert!(rej.output.contains("ref_not_found"), "{}", rej.output);

        // 2. Create plan with modify intent on plain txt (unknown capabilities) -> passes!
        let pass_unknown = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "test goal",
                "steps": [{
                    "title": "step 1",
                    "kind": "change",
                    "refs": [{"path": "notes.txt", "symbol": "anything", "intent": "modify"}]
                }]
            }),
        );
        assert!(pass_unknown.ok, "unknown must pass: {}", pass_unknown.output);

        // 3. Add step with create intent on an already existing symbol -> rejected!
        let rej_create = plan_op(
            &mut ctx,
            &json!({
                "op": "add",
                "title": "step 2",
                "kind": "change",
                "refs": [{"path": "src/calc.rs", "symbol": "add", "intent": "create"}]
            }),
        );
        assert!(!rej_create.ok, "should reject existing ref on create intent");
        assert!(rej_create.output.contains("ref_collision"), "{}", rej_create.output);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pre_edit_warning_emitted_for_unindexed_symbol() {
        let (mut ctx, dir) = proj();
        fs::write(
            dir.join("src/calc.rs"),
            "pub fn add(a: i32, b: i32) -> i32 {\n    let dummy = 1;\n    a + b\n}\n",
        )
        .unwrap();

        let mut store = crate::agent::graph::SqliteGraphStore::open(&dir).unwrap();
        crate::agent::graph_index::index_project(&mut store, &dir).unwrap();

        // read first to satisfy read guard
        execute(&mut ctx, "read", &json!({"file_path": "src/calc.rs"}));

        // Edit an unindexed identifier `dummy`
        let out = execute(
            &mut ctx,
            "edit",
            &json!({
                "file_path": "src/calc.rs",
                "old_string": "dummy",
                "new_string": "real_val"
            }),
        );
        assert!(out.ok, "edit must succeed");
        assert!(
            out.output.contains("warning: symbol 'dummy' not in index for this file"),
            "output was: {}",
            out.output
        );

        // Edit a known indexed symbol `add`
        execute(&mut ctx, "read", &json!({"file_path": "src/calc.rs"}));
        let out2 = execute(
            &mut ctx,
            "edit",
            &json!({
                "file_path": "src/calc.rs",
                "old_string": "add",
                "new_string": "plus"
            }),
        );
        assert!(out2.ok, "edit must succeed");
        assert!(
            !out2.output.contains("warning: symbol 'add' not in index"),
            "should not warn for indexed symbol: {}",
            out2.output
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dod_symbol_to_decision_to_original_record() {
        let (mut ctx, dir) = proj();

        // 1. Create a Rust source file defining Session
        fs::write(
            dir.join("src/session.rs"),
            "pub struct Session {\n    pub id: String,\n}\n",
        )
        .unwrap();

        // 2. Create diary file in .sqwai/memory/2026-09-10.md referencing `Session`
        let mem_dir = dir.join(".sqwai/memory");
        fs::create_dir_all(&mem_dir).unwrap();
        let diary_content = r#"
## 12:00 · session sess_1 · plan 01J123 · "Session storage"

### Decisions
- We decided that `Session` must persist todos into disk. (j#42)
"#;
        fs::write(mem_dir.join("2026-09-10.md"), diary_content).unwrap();

        // 3. Create journal with note record j#42
        let journal_dir = dir.join(".sqwai/journal");
        fs::create_dir_all(&journal_dir).unwrap();
        let note_record = json!({
            "seq": 42,
            "ts": "2026-09-10T12:00:00Z",
            "step": "1",
            "plan": "01J123",
            "agent": "main",
            "kind": "note",
            "by": "model",
            "note": "decision",
            "text": "We decided that `Session` must persist todos into disk."
        });
        fs::write(journal_dir.join("sess_1.jsonl"), format!("{note_record}\n")).unwrap();

        // Index the project
        let mut store = crate::agent::graph::SqliteGraphStore::open(&dir).unwrap();
        crate::agent::graph_index::index_project(&mut store, &dir).unwrap();

        // 4. graph_query tool execution:
        // Transition: Symbol -> Decision
        let gq_out = execute(
            &mut ctx,
            "graph_query",
            &json!({
                "node": "Session",
                "direction": "incoming",
                "relations": ["about"]
            }),
        );
        assert!(gq_out.ok, "graph_query must succeed: {}", gq_out.output);
        assert!(
            gq_out.output.contains("mem:2026-09-10#12-00:decision:1"),
            "must find incoming about edge from decision node: {}",
            gq_out.output
        );

        // 5. Verify the decision node points to original journal ref j#42 via recall
        let recall_out = execute(
            &mut ctx,
            "recall",
            &json!({"query": "persist todos"}),
        );
        assert!(recall_out.ok, "recall must succeed: {}", recall_out.output);
        assert!(
            recall_out.output.contains("j#42"),
            "recall must surface original journal ref j#42: {}",
            recall_out.output
        );

        // 6. Transition: Decision -> Original record in journal
        let journal_out = execute(
            &mut ctx,
            "journal",
            &json!({
                "session": "sess_1",
                "query": "persist todos"
            }),
        );
        assert!(journal_out.ok, "journal tool must succeed: {}", journal_out.output);
        assert!(
            journal_out.output.contains("#42"),
            "journal tool must retrieve the original record #42: {}",
            journal_out.output
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn graph_query_tool_unresolved_start_returns_structured_error() {
        let (mut ctx, dir) = proj();
        let mut store = crate::agent::graph::SqliteGraphStore::open(&dir).unwrap();
        crate::agent::graph_index::index_project(&mut store, &dir).unwrap();

        let out = execute(
            &mut ctx,
            "graph_query",
            &json!({
                "node": "nonexistent::FooBar"
            }),
        );
        assert!(!out.ok, "must fail for nonexistent start");
        assert!(out.output.contains("unresolved_start"), "must contain unresolved_start code: {}", out.output);
        assert!(out.output.contains("hint"), "must contain hint: {}", out.output);

        fs::remove_dir_all(&dir).ok();
    }
}
