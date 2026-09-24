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
use std::sync::{Mutex, OnceLock};

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
    /// Override for the checkpoint chain only (§2.2.4): a child session's
    /// snapshots land on its parent's chain, so the parent's `/undo` sees
    /// the child's step boundaries and bash mutations. `None` means the own
    /// chain. Job isolation, read-guard and journal identity all stay on
    /// `session_id` — only the shadow chain is shared.
    pub checkpoint_session: Option<String>,
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
    /// Write scope for a writer subagent (`None` = unrestricted): every
    /// file mutation must sit inside these project-relative roots. Taken
    /// from the spawn registry at construction; read-only children and
    /// the main agent never set it.
    pub subagent_write_paths: Option<Vec<String>>,
    /// H1 reflector executor (§12.7): verify-only context. The dispatcher
    /// refuses every tool outside [`REFLECTOR_TOOLS`] plus mutating `bash`,
    /// with an honest message. Unlike `read_only` (a lock held elsewhere)
    /// this is a role: there is nothing to wait for or `--force` past.
    pub reflector: bool,
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
            checkpoint_session: None,
            files_read: HashMap::new(),
            journal: Vec::new(),
            plan_limits: crate::config::PlanConfig::default(),
            context_limit: 0,
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            shadow_store: crate::config::ShadowStore::Local,
            current_step: None,
            subagent_step: None,
            subagent_write_paths: None,
            reflector: false,
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

    /// Whose shadow chain snapshots go to (child → parent). Everything else
    /// keeps using `session_id`.
    pub fn checkpoint_chain(&self) -> &str {
        self.checkpoint_session
            .as_deref()
            .unwrap_or(&self.session_id)
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
            description: "Wait for a background command and read its output. ALWAYS prefer wait_secs (0-60): it parks until the job exits or the timeout lapses — intermediate output never wakes it early — then returns everything accumulated. One call instead of a poll loop. Reads without it on a running job are free twice, then force-waited (15s, then 30s). Output is incremental: the first read returns the tail, later reads return only bytes appended since the previous read (from_start=true re-reads the tail). Only this session's jobs are visible here. A finished job is reported once with its exit code, then cleaned up. Without id: a list of this session's background jobs with their commands and log paths.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "job id from bash background=true"},
                    "tail": {"type": "integer", "description": "bytes of output to return (default 10000, max 50000)"},
                    "wait_secs": {"type": "integer", "description": "PREFERRED: park up to N seconds (max 60) until the job exits; returns everything accumulated. Never wakes early on output."},
                    "from_start": {"type": "boolean", "description": "re-read the tail from scratch instead of the delta"}
                }
            }),
        },
        ToolDef {
            name: "bash_kill",
            kind: Kind::ReadOnly,
            description: "Stop one of this session's background commands started with bash background=true, including everything it spawned. Jobs from other sessions are invisible here. Reports the job's command and final state.",
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
            description: "Delegate one or more focused tasks to child agents. Children inherit the current Plan/Act mode; up to 8 tasks are accepted, at most 4 run concurrently, and child agents cannot create further subagents. A child that produces nothing for 600s is cancelled and reported as timed out. Separate subagent calls in one turn also run concurrently. Children are read-only by default; a task object with write:true and paths:[...] declares a writer scoped to those roots (non-empty, non-overlapping with sibling writers) — writes outside the scope are refused.",
            parameters: json!({"type":"object","properties":{"task":{"type":["string","object"],"description":"one focused child task: a string (read-only), or an object with task|prompt plus write:true and paths:[...] to declare a scoped writer"},"tasks":{"type":"array","items":{"anyOf":[{"type":"string"},{"type":"object","properties":{"task":{"type":"string"},"prompt":{"type":"string"},"description":{"type":"string"},"write":{"type":"boolean","description":"allow file writes, scoped to paths"},"paths":{"type":"array","items":{"type":"string"},"description":"write scope roots, required with write:true"}},"additionalProperties":true}]},"minItems":1,"maxItems":8,"description":"focused child tasks to run concurrently (strings, or objects with task|prompt)"}},"anyOf":[{"required":["task"]},{"required":["tasks"]}]}),
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
finish, block, unblock, cancel, add, split, verify, complete, show, block_plan. Call \
show first if you are unsure of the current step ids. The host owns the goal, the constraints, \
acceptance status, validation and evidence; to change the goal, propose the full updated plan with \
propose_plan instead. finish records completion of the step's work with a summary and does not \
by itself establish that acceptance criteria passed; it requires host-recorded evidence since \
start only in strict mode ([plan] strict), and rejections return a code and hint to follow. \
A manual: acceptance can be \
waived only by the user, never verified by the model. complete requires every step closed and \
every acceptance validation passed or waived with fresh receipts. block_plan surrenders \
an impossible task: use it when the spec contradicts the tests (or itself) instead of gaming \
either side — quote the conflict in reason. A blocked plan is an honest result, never a failure. \
Never invent evidence identifiers.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "op": {"type": "string", "enum": [
                        "create", "start", "finish", "block", "unblock", "cancel",
                        "add", "split", "verify", "complete", "show", "block_plan"
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
                                "refs": {"type": "array", "items": {"type": ["string", "object"]}, "description": "what the step touches: plain \"path[::symbol]\" means modify, or {\"path\", \"symbol\", \"intent\": \"modify|create|remove\"}"}
                            },
                            "required": ["title"]
                        }
                    },
                    "after": {"type": "string", "description": "add: insert after this step id"},
                    "title": {"type": "string", "description": "add: new step title"},
                    "refs": {"type": "array", "items": {"type": ["string", "object"]}, "description": "what the step touches: plain \"path[::symbol]\" means modify, or {\"path\", \"symbol\", \"intent\": \"modify|create|remove\"}"},
                    "summary": {"type": "string", "description": "finish: what was done, where, and any remaining limitations"},
                    "reason": {"type": "string", "description": "block / cancel / block_plan: the quoted conflict for block_plan"},
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

/// A bash call that only inspects: every pipeline/chain segment starts with
/// a known read-only verb and nothing redirects into a file. Advisory
/// classification for plan discipline ONLY — never a safety boundary (a
/// hostile command line can spell reads that write; approvals still guard
/// real damage). Fail-closed: anything unrecognized stays mutating.
pub fn is_readonly_bash(name: &str, args: &Value) -> bool {
    if name != "bash" {
        return false;
    }
    let Some(command) = args.get("command").and_then(|value| value.as_str()) else {
        return false;
    };
    readonly_command(command.trim())
}

/// PowerShell read-only verb prefixes (`Get-Process`, `Where-Object`, bare
/// `select`/`sort` aliases included by prefix).
const READONLY_PS_VERBS: &[&str] = &[
    "get-", "select-", "where-", "sort-", "format-", "measure-", "compare-", "test-",
    "resolve-", "group-",
];

/// cmd read-only heads. The query-capable ones (`schtasks`, `reg`, `sc`)
/// additionally require a `query` subcommand (see below).
const READONLY_CMD_HEADS: &[&str] = &[
    "netstat",
    "tasklist",
    "schtasks",
    "reg",
    "sc",
    "driverquery",
    "systeminfo",
    "ipconfig",
    "hostname",
    "ver",
    "whoami",
    "echo",
    "dir",
    "type",
    "find",
    "findstr",
    "more",
    "tree",
];

fn readonly_command(command: &str) -> bool {
    if command.is_empty() {
        return false;
    }
    // wrapper shells carry the real command in a quoted -Command/-c string;
    // anything else in wrapper position is unrecognized by construction
    let inner = match shell_wrapper_inner(command) {
        Some(inner) => inner,
        None => return false,
    };
    let inner = inner.trim();
    if inner.is_empty() || inner.contains("$(") {
        return false;
    }
    // stderr merge is hygiene, not a write; any other redirect is
    let unredirected = inner.replace("2>&1", "");
    if unredirected.contains('>') {
        return false;
    }
    split_shell_segments(&unredirected)
        .iter()
        .all(|segment| readonly_segment(segment))
}

/// `powershell -Command "..."` / `pwsh -c '...'` / `cmd /c "..."` yield the
/// inner command line; a bare (non-wrapper) command yields itself; anything
/// unrecognized yields `None` (fail closed).
fn shell_wrapper_inner(command: &str) -> Option<&str> {
    let head = command
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase();
    let bare = head
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim_matches('"');
    if !matches!(bare, "powershell" | "pwsh" | "cmd") {
        return Some(command);
    }
    let quote = command.find('"').or_else(|| command.find('\''))?;
    let inner = command[quote + 1..].trim_end();
    let inner = inner
        .strip_suffix('"')
        .or_else(|| inner.strip_suffix('\''))
        .unwrap_or(inner);
    Some(inner)
}

/// Split a command line on pipeline and chain operators. Quotes are NOT
/// tracked: a `|` inside quotes splits wrongly and fails closed downstream,
/// which is the safe direction.
fn split_shell_segments(command: &str) -> Vec<&str> {
    command
        .split(['|', '&', ';'])
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect()
}

fn readonly_segment(segment: &str) -> bool {
    let head = segment
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase();
    if head.is_empty() {
        return false;
    }
    if READONLY_PS_VERBS.iter().any(|verb| head.starts_with(verb)) {
        return true;
    }
    if READONLY_CMD_HEADS.contains(&head.as_str()) {
        // query-capable tools read only with the query subcommand
        if matches!(head.as_str(), "schtasks" | "reg" | "sc") {
            return segment.to_lowercase().contains("query");
        }
        return true;
    }
    false
}

/// File path a call targets, if any — recorded on the journal `tool_call`
/// record so re-reads (same path read twice: context-loss symptom) can be
/// told apart from plan discipline. Raw value, no normalization; analysis
/// normalizes. Empty counts as absent.
pub fn call_path(name: &str, args: &Value) -> Option<String> {
    let key = match name {
        "read" | "write" | "edit" | "multi_edit" => "file_path",
        "ls" | "outline" => "path",
        _ => return None,
    };
    args[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// one-line description of a call's arguments for the live TUI row
pub fn call_summary(name: &str, args: &Value) -> String {
    let s = |k: &str| args[k].as_str().unwrap_or_default().to_string();
    match name {
        "ls" => s("path"),
        "read" | "write" | "edit" | "multi_edit" => s("file_path"),
        "bash" => s("command"),
        "bash_output" => {
            // wait_secs/from_start visible right in the row: no need to
            // expand the call to see whether the model waited or polled
            let base = args["id"]
                .as_u64()
                .map(|id| format!("job {id}"))
                .unwrap_or_else(|| "list".to_string());
            let mut extra = String::new();
            if args["wait_secs"].as_u64().unwrap_or(0) > 0 {
                extra.push_str(&format!(
                    " wait {}s",
                    args["wait_secs"].as_u64().unwrap_or(0)
                ));
            }
            if args["from_start"].as_bool().unwrap_or(false) {
                extra.push_str(" from start");
            }
            format!("{base}{extra}")
        }
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
/// prefix cache to hit. The set is mode-independent for the same reason;
/// Plan mode refuses mutating calls at dispatch instead of hiding them.
pub fn tool_names() -> Vec<String> {
    let mut names: Vec<String> = defs().into_iter().map(|d| d.name.to_string()).collect();
    names.sort();
    names
}

/// Tools the H1 executor may see. Read-only inspection plus gated `bash`;
/// everything else is refused twice — absent from the specs, refused in
/// dispatch. `plan`, `note`, `memory_*`, `web*`, job control and all
/// writers are out by design (§12.7: the reflector changes nothing).
pub const REFLECTOR_TOOLS: &[&str] = &[
    "read",
    "ls",
    "glob",
    "grep",
    "outline",
    "ast_grep",
    "resolve_ref",
    "recall",
    "journal",
    "think",
    "bash",
    "git_status",
    "git_log",
    "git_diff",
    "git_show",
    "step_diff",
];

/// Specs for the H1 executor: the [`REFLECTOR_TOOLS`] subset, sorted like
/// [`tool_specs`] (stable order keeps the prompt cache-friendly).
pub fn reflector_specs() -> Vec<crate::providers::ToolSpec> {
    let mut specs: Vec<crate::providers::ToolSpec> = defs()
        .into_iter()
        .filter(|d| REFLECTOR_TOOLS.contains(&d.name))
        .map(|d| crate::providers::ToolSpec {
            name: d.name.to_string(),
            description: d.description.to_string(),
            parameters: d.parameters,
        })
        .collect();
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    specs
}

pub fn tool_specs(_plan_mode: bool) -> Vec<crate::providers::ToolSpec> {    // One schema set in every mode. The tool block is part of the request
    // prefix, so a mode-dependent set re-keys the cache on every Plan/Act
    // toggle; masking (Manus-style) would cost the same. Plan mode is
    // enforced by the dispatcher instead (`is_mutating_call` at dispatch
    // refuses mutating calls with an honest message, `git_branch` actions
    // included) — which is where the authority already lived.
    // G0 baseline (§8.2): the durable machinery is invisible — no plan,
    // notes, journal projection, or durable memory tools.
    let baseline = crate::bench::baseline();
    let mut specs: Vec<crate::providers::ToolSpec> = defs()
        .into_iter()
        .filter(|d| {
            !baseline
                || !matches!(
                    d.name,
                    "plan" | "propose_plan" | "note" | "journal" | "memory_propose" | "memory_read"
                )
        })
        .map(|d| crate::providers::ToolSpec {
            name: d.name.to_string(),
            description: d.description.to_string(),
            parameters: d.parameters,
        })
        .collect();
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    specs
}

/// Merge external (MCP) schemas into the built-in set. The merged set is
/// re-sorted as a whole: MCP order follows server registration and server
/// responses, so appending them after the sorted built-ins would re-key the
/// tool prefix (and the cache behind it) whenever servers flake or reorder.
pub fn merge_specs(
    mut base: Vec<crate::providers::ToolSpec>,
    extra: &[crate::providers::ToolSpec],
) -> Vec<crate::providers::ToolSpec> {
    base.extend(extra.iter().cloned());
    base.sort_by(|a, b| a.name.cmp(&b.name));
    base
}

/// Mid-trim for tool output: keep a third of the budget at the head and the
/// rest at the tail, cut the middle. The head carries the command echo and
/// first lines, the tail the errors and the result; the middle is the least
/// informative slice (lost-in-the-middle). Char-boundary safe; text that
/// fits passes through untouched, with no marker.
pub fn trim_middle(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let head_len = max_chars / 3;
    let tail_len = max_chars - head_len;
    let head: String = text.chars().take(head_len).collect();
    let tail: String = text.chars().skip(total - tail_len).collect();
    format!(
        "{}\n…(output truncated: {} chars omitted)\n{}",
        head.trim_end(),
        total - head_len - tail_len,
        tail.trim_start()
    )
}

/// Decode child-process output. UTF-8 when valid; otherwise the Windows
/// console codepage (cp866 on RU Windows — cmd/powershell system messages).
/// Plain `from_utf8_lossy` turned those into ����, poisoning baselines and
/// reports. Line-wise, so a UTF-8 program line next to a cp866 system error
/// line each decodes correctly. File content never comes here (source files
/// are UTF-8 by contract; lossy is right for them).
pub fn decode_child_output(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    let mut out = String::new();
    for line in bytes.split(|b| *b == b'\n') {
        if !out.is_empty() {
            out.push('\n');
        }
        match std::str::from_utf8(line) {
            Ok(text) => out.push_str(text),
            Err(_) => {
                for b in line {
                    out.push(cp866_char(*b));
                }
            }
        }
    }
    out
}

/// One cp866 byte to char. Cyrillic ranges first (the observed mojibake),
/// then box drawing and symbols; ASCII passes through.
fn cp866_char(byte: u8) -> char {
    match byte {
        0x00..=0x7F => byte as char,
        0x80..=0x9F => char::from_u32(0x0410 + (byte - 0x80) as u32).unwrap_or('�'),
        0xA0..=0xAF => char::from_u32(0x0430 + (byte - 0xA0) as u32).unwrap_or('�'),
        0xB0..=0xBF => [
            '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜',
            '╛', '┐',
        ][(byte - 0xB0) as usize],
        0xC0..=0xCF => [
            '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═',
            '╬', '╧',
        ][(byte - 0xC0) as usize],
        0xD0..=0xDF => [
            '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌',
            '▐', '■',
        ][(byte - 0xD0) as usize],
        0xE0..=0xEF => char::from_u32(0x0440 + (byte - 0xE0) as u32).unwrap_or('�'),
        0xF0 => 'Ё',
        0xF1 => 'ё',
        0xF2 => 'Є',
        0xF3 => 'є',
        0xF4 => 'Ї',
        0xF5 => 'ї',
        0xF6 => 'Ў',
        0xF7 => 'ў',
        0xF8 => '°',
        0xF9 => '∙',
        0xFA => '·',
        0xFB => '√',
        0xFC => '№',
        0xFD => '¤',
        0xFE => '■',
        0xFF => '\u{a0}',
    }
}

/// Decode check: cp866 "Привет" (П=0x8F, р=0xE0, и=0xA8, в=0xA2,
/// е=0xA5, т=0xE2) must round-trip, valid UTF-8 (emoji included) passes
/// through untouched, and mixed lines decode each in its own encoding.
#[test]
fn decode_child_output_handles_console_codepage() {
    assert_eq!(decode_child_output(&[0x8F, 0xE0, 0xA8, 0xA2, 0xA5, 0xE2]), "Привет");
    assert_eq!(decode_child_output("ok 🔥 ЕС".as_bytes()), "ok 🔥 ЕС");
    let mut mixed = b"done\n".to_vec();
    mixed.extend_from_slice(&[0x8E, 0xE8, 0xA8, 0xA1, 0xAA, 0xA0]); // "Ошибка" in cp866
    assert_eq!(decode_child_output(&mixed), "done\nОшибка");
    assert_eq!(decode_child_output(&[0xB3]), "│");
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

/// `bash` for the H1 executor: Safe commands run blocking with a cap;
/// anything else refuses. Headless means no approvals, no background.
fn reflector_bash(ctx: &mut ToolCtx, command: &str, timeout: Option<u64>) -> Outcome {
    if command.trim().is_empty() {
        return Outcome::err("bash requires a non-empty 'command' argument");
    }
    match crate::agent::safety::classify_for(crate::agent::shell::ShellKind::detect(), command) {
        crate::agent::safety::Verdict::Safe => {
            exec::bash(ctx, command, timeout.or(Some(120)), false)
        }
        crate::agent::safety::Verdict::Blocked(reason)
        | crate::agent::safety::Verdict::NeedsApproval(reason) => Outcome::err(
            serde_json::json!({
                "ok": false,
                "code": "reflector_read_only",
                "reason": format!("reflector refuses this command ({reason}): verify, do not change"),
            })
            .to_string(),
        ),
    }
}

/// (id, owning session, command) of background jobs whose processes are
/// still alive. The undo preflight (§2.5, S1) refuses a restore while any
/// of these run — the writer lock stops in-process dispatch, but an
/// already-running shell would keep writing mid-restore.
pub(crate) fn bg_running_commands() -> Vec<(u64, String, String)> {
    exec::running_commands()
}

/// Project-relative, lexically cleaned mutation targets of a file-writing
/// call: `file_path` for write/edit/multi_edit, `+++` files for patch.
/// Unresolvable or empty spellings yield nothing — the tool's own
/// validation reports those, not the scope gates.
fn mutation_target_paths(ctx: &ToolCtx, name: &str, args: &Value) -> Vec<String> {
    let raws: Vec<String> = if name == "patch" {
        let patch = args["patch"].as_str().unwrap_or_default();
        if patch.trim().is_empty() {
            return Vec::new();
        }
        git::extract_patch_files(&ctx.root, patch)
    } else {
        let raw = args["file_path"].as_str().unwrap_or_default();
        if raw.trim().is_empty() {
            return Vec::new();
        }
        vec![raw.to_string()]
    };
    raws
        .into_iter()
        .filter_map(|raw| {
            let resolved = ctx.resolve(&raw).ok()?;
            let rel = resolved
                .strip_prefix(&ctx.root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| raw.clone());
            Some(lexical_clean(&rel))
        })
        .collect()
}

/// First frozen check input a file mutation would touch, if any. Resolves
/// for the jail verdict, then compares lexically cleaned relative paths so
/// `..` spellings cannot dodge the freeze. Fail-open on an unreadable
/// store: the journal heals the plan, and an infra hiccup must not brick
/// writes.
fn frozen_input_hit(ctx: &ToolCtx, name: &str, args: &Value) -> Option<String> {
    let plan = plan::open_active_for_session(&ctx.root, Some(&ctx.session_id)).ok()??;
    let frozen = plan::frozen_input_paths(&plan);
    if frozen.is_empty() {
        return None;
    }
    mutation_target_paths(ctx, name, args)
        .into_iter()
        .find(|clean| frozen.iter().any(|f| f == clean))
}

/// Lexically clean a relative path: drop `.`, resolve `..` against the
/// stack. No filesystem access — the jail verdict already came from
/// `resolve`.
fn lexical_clean(path: &str) -> String {
    let mut stack: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                if stack.last().is_some_and(|top| *top != "..") {
                    stack.pop();
                } else {
                    stack.push("..");
                }
            }
            _ => stack.push(comp),
        }
    }
    stack.join("/")
}

/// Frozen input a shell command would write, if any. Best-effort heuristic,
/// documented as such: a frozen path token plus a write shape (redirect,
/// `tee`, in-place `sed`, copy/move onto it, patch application). Reading a
/// frozen file never matches. What slips past still meets the receipt-time
/// hash comparison — this gate steers early, that one judges.
pub(crate) fn frozen_input_command_hit(
    root: &Path,
    session_id: &str,
    command: &str,
) -> Option<String> {
    let plan = plan::open_active_for_session(root, Some(session_id)).ok()??;
    let frozen = plan::frozen_input_paths(&plan);
    if frozen.is_empty() {
        return None;
    }
    // path-ish tokens, quotes stripped, lexically cleaned
    let tokens: Vec<String> = command
        .split(|c: char| {
            c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')' | '$' | '`' | '\'' | '"')
        })
        .filter(|t| t.contains('/'))
        .map(|t| lexical_clean(t.trim_matches(|c| c == '\'' || c == '"')))
        .collect();
    if tokens.is_empty() {
        return None;
    }
    let frozen_token = || {
        tokens
            .iter()
            .find(|t| frozen.iter().any(|f| f == *t))
            .cloned()
    };
    // `> file`, `>> file`, `2>file`: the token after a redirect operator
    // (`2>&1` merges streams — no file — so `>` before `&` never counts)
    let mut words = command.split_whitespace().peekable();
    while let Some(word) = words.next() {
        let op = word.trim_matches(|c| c == '\'' || c == '"');
        let redirect = op == ">"
            || op == ">>"
            || (op.ends_with('>')
                && op[..op.len() - 1].chars().all(|c| c.is_ascii_digit()));
        if !redirect {
            continue;
        }
        if let Some(dest) = words.peek() {
            let clean = lexical_clean(dest.trim_matches(|c| c == '\'' || c == '"'));
            if clean.starts_with('&') {
                continue;
            }
            if frozen.iter().any(|f| f == &clean) {
                return Some(frozen_input_reason(&clean));
            }
        }
    }
    // `tee` writes every file arg; `patch`/`git apply` scatter writes the
    // tokens cannot resolve — any frozen token in such a command asks first
    let lower = command.to_lowercase();
    if lower.contains("tee") || lower.contains("git apply") || lower.contains("patch ") {
        if let Some(hit) = frozen_token() {
            return Some(frozen_input_reason(&hit));
        }
    }
    // in-place editors and copy/move: a frozen token beside the shape asks
    if (lower.contains("sed") && lower.contains("-i"))
        || ["cp", "mv", "install", "rsync", "dd", "truncate"]
            .iter()
            .any(|w| lower.split_whitespace().any(|t| t == *w))
    {
        if let Some(hit) = frozen_token() {
            return Some(frozen_input_reason(&hit));
        }
    }
    None
}

fn frozen_input_reason(path: &str) -> String {
    format!(
        "edits frozen check input '{path}': the test/fixture was hashed at plan time, approve to change the check itself"
    )
}

/// Writer scopes of live subagents, keyed by child session. Written at
/// spawn (after scope validation), taken once at child-context
/// construction — single take, so a crashed spawn cannot poison anything
/// later (session ids are unique per spawn).
fn subagent_scopes() -> &'static Mutex<HashMap<String, Vec<String>>> {
    static SCOPES: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();
    SCOPES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a writer child's path scope. Called at spawn, after the
/// write flag and the sibling-overlap checks passed.
pub(crate) fn register_subagent_scope(session: &str, paths: Vec<String>) {
    subagent_scopes()
        .lock()
        .unwrap()
        .insert(session.to_string(), paths);
}

/// Take a registered scope for child-context construction.
pub(crate) fn take_subagent_scope(session: &str) -> Option<Vec<String>> {
    subagent_scopes().lock().unwrap().remove(session)
}

/// True when `path` (project-relative, forward slashes) sits inside one
/// of the scope roots: equal, nested under a root, or the whole tree (`.`).
fn in_write_scope(path: &str, scope: &[String]) -> bool {
    scope
        .iter()
        .any(|root| path == root || path.starts_with(&format!("{root}/")) || root == ".")
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
    // S1 writer lock: an undo restore is reverting the tree right now.
    // In-process dispatch must not add writes under it (a second sqwai
    // instance is already covered by the read-only guard above).
    if crate::agent::undo_guard::restore_active()
        && matches!(name, "write" | "edit" | "multi_edit" | "patch" | "bash")
    {
        return Outcome::err(
            serde_json::json!({
                "ok": false,
                "code": "writer_locked",
                "reason": "an undo restore is reverting the tree; retry the mutation after it completes",
            })
            .to_string(),
        );
    }
    // H1 reflector executor (§12.7): verify-only. Tools outside
    // REFLECTOR_TOOLS never reach dispatch in practice (absent from the
    // specs), but a hallucinated name must refuse loudly rather than run.
    // `bash` runs only when the safety classifier calls it Safe — approvals
    // cannot exist headless, so NeedsApproval refuses like Blocked — and
    // never in background (no job-registry pollution across the turn).
    if ctx.reflector {
        if name == "bash" {
            return reflector_bash(
                ctx,
                args["command"].as_str().unwrap_or_default(),
                args["timeout"].as_u64(),
            );
        }
        if !REFLECTOR_TOOLS.contains(&name) {
            return Outcome::err(
                serde_json::json!({
                    "ok": false,
                    "code": "reflector_read_only",
                    "reason": format!(
                        "reflector is read-only: '{name}' is not a verification tool — verify the tree, do not change it"
                    ),
                })
                .to_string(),
            );
        }
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
    // Frozen check inputs: editing a test/fixture the active plan froze at
    // create changes the check, not the code. Refuse; the user takes
    // responsibility by waiving the item (/plan waive) or surrendering a
    // contradictory spec (block_plan). New files stay writable — rung 2
    // lives on that.
    if matches!(name, "write" | "edit" | "multi_edit" | "patch")
        && let Some(path) = frozen_input_hit(ctx, name, args)
    {
        return Outcome::err(
            serde_json::json!({
                "ok": false,
                "code": "frozen_input",
                "reason": format!(
                    "'{path}' is a frozen check input: editing it changes the check, not the code under test"
                ),
                "hint": "restore the file, have the user waive the acceptance item (/plan waive), or surrender a contradictory spec with block_plan",
            })
            .to_string(),
        );
    }
    // Writer-subagent scope: a child declared its roots at spawn, and every
    // file mutation must sit inside them. Read-only children never reach
    // here (refused above); the main agent carries no scope.
    if ctx.subagent_step.is_some()
        && let Some(allowed) = ctx.subagent_write_paths.as_ref()
        && matches!(name, "write" | "edit" | "multi_edit" | "patch")
    {
        let outside = mutation_target_paths(ctx, name, args)
            .into_iter()
            .find(|target| !in_write_scope(target, allowed));
        if let Some(path) = outside {
            return Outcome::err(
                serde_json::json!({
                    "ok": false,
                    "code": "subagent_scope",
                    "reason": format!(
                        "'{path}' is outside this subagent's declared write scope ({})",
                        allowed.join(", ")
                    ),
                    "hint": "stay inside the spawned scope, or spawn with wider paths",
                })
                .to_string(),
            );
        }
    }
    let mut outcome = match name {
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
        "bash_kill" => exec::bash_kill(ctx, args),
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
                        let mut out =
                            format!("Recall results for '{query}' ({} matches):\n", items.len());
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
            let max_output_tokens = args["max_output_tokens"].as_u64().unwrap_or(2000) as usize;
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
                        out.push_str(&format!("  - {} --({})--> {}\n", e.from, e.kind, e.to));
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
                        let hint = err_str
                            .strip_prefix("unresolved_start:")
                            .unwrap_or(&err_str)
                            .trim();
                        Outcome::err(
                            serde_json::to_string_pretty(&serde_json::json!({
                                "ok": false,
                                "code": "unresolved_start",
                                "node": node,
                                "hint": hint,
                            }))
                            .unwrap_or_else(|_| format!("unresolved start node '{node}': {hint}")),
                        )
                    } else if err_str.contains("ambiguous_start:") {
                        let hint = err_str
                            .strip_prefix("ambiguous_start:")
                            .unwrap_or(&err_str)
                            .trim();
                        Outcome::err(
                            serde_json::to_string_pretty(&serde_json::json!({
                                "ok": false,
                                "code": "ambiguous_start",
                                "node": node,
                                "hint": hint,
                            }))
                            .unwrap_or_else(|_| format!("ambiguous start node '{node}': {hint}")),
                        )
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
                // Scoped to this session: sequence numbers restart per file.
                let open = crate::agent::journal::Journal::open_assumptions_in(
                    &ctx.root,
                    &ctx.session_id,
                    None,
                )
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
    };
    lint_write_path(ctx, name, &mut outcome);
    outcome
}

/// Z + AF write-path gates (warn-layer only, §2.1.9): scope-check the
/// touched paths against the holding step's refs, and scan the unified
/// diff for test-shaped literals. Runs after a successful file mutation.
/// `bash` is excluded — shell-written bytes leave no per-file diff to
/// attribute or scan (pre/post snapshots and checkpoints cover them, and
/// the model is told so nowhere: the gap is documented, not silent).
fn lint_write_path(ctx: &ToolCtx, name: &str, outcome: &mut Outcome) {
    if !outcome.ok || !matches!(name, "write" | "edit" | "multi_edit" | "patch") {
        return;
    }
    let mut touched: Vec<&str> = Vec::new();
    if let Some(diff) = outcome.file_diff.as_ref() {
        touched.push(diff.path.as_str());
    }
    for diff in &outcome.file_diffs {
        touched.push(diff.path.as_str());
    }
    if touched.is_empty() {
        return;
    }
    // The holding step's refs: the child's inherited context, or the
    // session's own active plan plus its current step. Anything
    // unresolvable means "no scope declared" — the lint stays silent.
    let refs: Vec<crate::plan::StepRef> = if let Some(inherited) = ctx.subagent_step.as_ref() {
        crate::plan::read_plan_file(&ctx.root, &inherited.plan_id)
            .and_then(|plan| {
                plan.steps
                    .into_iter()
                    .find(|step| step.id == inherited.step_id)
            })
            .map(|step| step.refs)
            .unwrap_or_default()
    } else if let Some(step_id) = ctx.current_step.as_deref() {
        crate::plan::open_active_for_session(&ctx.root, Some(ctx.session_id.as_str()))
            .ok()
            .flatten()
            .and_then(|plan| {
                plan.steps
                    .into_iter()
                    .find(|step| step.id.as_str() == step_id)
            })
            .map(|step| step.refs)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut warnings = crate::agent::lint::scope_warnings(&refs, &touched);
    if let Some(diff) = outcome.diff.as_deref() {
        let label = touched.first().copied().unwrap_or("unknown file");
        warnings.extend(crate::agent::lint::hardcode_warnings(diff, label));
    }
    for warning in warnings {
        outcome.output.push('\n');
        outcome.output.push_str(&warning);
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
        let open = match crate::agent::journal::Journal::open_assumptions_in(
            &ctx.root,
            &ctx.session_id,
            None,
        ) {
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
                if is_create_intent && let crate::agent::graph::ResolveRefResult::Found { .. } = res
                {
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
    Ok(())
}

/// Create-time validation for typed constraints (§2.1.10): empty payloads
/// settle or block nothing, `path:` roots must resolve, `ast:` patterns
/// must compile. Rejects while the model can still rewrite the item.
fn validate_typed_constraints(
    ctx: &mut ToolCtx,
    constraints: &[String],
) -> Result<(), plan::Rejection> {
    for text in constraints {
        match plan::classify_constraint(text) {
            plan::ConstraintKind::Plain(_) | plan::ConstraintKind::ForbidImport(_) => {}
            plan::ConstraintKind::ForbidCmd(pattern) => {
                if pattern.trim().is_empty() {
                    return Err(plan::Rejection::new(
                        "empty_constraint",
                        "forbid-cmd: names no pattern".to_string(),
                        "name the command shape to forbid, or drop the item".to_string(),
                    ));
                }
            }
            plan::ConstraintKind::Ast(pattern) => {
                if pattern.trim().is_empty() {
                    return Err(plan::Rejection::new(
                        "empty_constraint",
                        "ast: names no pattern".to_string(),
                        "give the tree-sitter pattern, or drop the item".to_string(),
                    ));
                }
                let outcome = astgrep::ast_grep(
                    ctx,
                    &serde_json::json!({"pattern": pattern, "path": ".", "max": 1}),
                );
                if !outcome.ok {
                    return Err(plan::Rejection::new(
                        "bad_constraint_pattern",
                        format!("ast: pattern does not compile: {pattern}"),
                        format!("fix the pattern — {}", outcome.output),
                    ));
                }
            }
            plan::ConstraintKind::Path(roots) => {
                if roots.is_empty() {
                    return Err(plan::Rejection::new(
                        "empty_constraint",
                        "path: names no roots".to_string(),
                        "name the roots the change must stay inside, or drop the item".to_string(),
                    ));
                }
                for root in roots {
                    if let Err(message) = ctx.resolve(root) {
                        return Err(plan::Rejection::new(
                            "bad_constraint_path",
                            format!("path: root '{root}' does not resolve: {message}"),
                            "name existing project paths (missing files are fine, escapes are not)"
                                .to_string(),
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Advisory AGENTS.md mining at create: restriction markers with no typed
/// constraint covering them earn one note line. Advisory only — never a
/// gate — and silent once the author formalized anything.
fn mine_constraint_candidates(root: &Path) -> Vec<String> {
    const MARKERS: &[&str] = &[
        "don't use",
        "do not use",
        "forbidden",
        "never use",
        "deprecated",
        "avoid using",
        "do not touch",
        "don't touch",
    ];
    let text = match std::fs::read_to_string(root.join("AGENTS.md")) {
        Ok(text) => text,
        Err(_) => return Vec::new(),
    };
    text.lines()
        .map(str::trim)
        .filter(|line| {
            let lower = line.to_lowercase();
            MARKERS.iter().any(|m| lower.contains(m))
        })
        .take(5)
        .map(|line| {
            line.chars()
                .take(120)
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|line| !line.is_empty())
        .collect()
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
    // host-only ops never reach the validator: session membership is joined
    // by the host when it spawns a child, not proposed by the model
    if matches!(op, plan::Op::Join { .. }) {
        return Outcome::err(
            "plan op rejected: join is host-only — sessions join a plan via plan start, not this op",
        );
    }
    let limits = plan::Limits {
        max_steps: ctx.plan_limits.max_steps,
    };

    let gate = if matches!(op, plan::Op::Complete) {
        validate_complete(ctx)
    } else {
        validate_evidence(&ctx.root, &op, Some(&ctx.session_id), ctx.plan_limits.strict)
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
                // Named verify commands (`cmd: $name`) expand here, against
                // the project's seeded map — an unknown name rejects the
                // create with the known list instead of burning a turn at
                // verify time on a command that never existed.
                let acceptance = match plan::substitute_verify_commands(
                    acceptance,
                    &crate::config::Config::project_verify_commands(&ctx.root),
                ) {
                    Ok(expanded) => expanded,
                    Err(unknown) => {
                        let hint = if unknown.known.is_empty() {
                            "no verify commands seeded — run /init or write the command out"
                                .to_string()
                        } else {
                            format!("known: {}", unknown.known.join(", "))
                        };
                        return rejection(plan::Rejection::new(
                            "unknown_verify",
                            format!(
                                "acceptance refers to unknown verify command(s): ${}",
                                unknown.names.join(", $")
                            ),
                            hint,
                        ));
                    }
                };
                match plan::create(goal, constraints, acceptance, steps, budget_limit, &limits) {
                    Ok(mut created) => {
                        for s in &created.steps {
                            if let Err(rej) = validate_plan_refs(&ctx.root, &s.refs, true) {
                                return rejection(rej);
                            }
                        }
                        // typed constraints are validated like refs: an empty
                        // payload, an unresolvable root, or an uncompilable
                        // pattern rejects the create while the model can
                        // still rewrite it — not at `complete`, when the
                        // work is already done.
                        if let Err(rej) = validate_typed_constraints(ctx, &created.constraints) {
                            return rejection(rej);
                        }
                        created.sessions = vec![ctx.session_id.clone()];
                        // §12.12: prove the cmd: checks discriminate, before
                        // anything has changed. Rung 4 freezes beside them.
                        let proof = capture_baselines(ctx, &created);
                        plan::set_baselines(&mut created, proof.slots.clone());
                        plan::set_snapshots(&mut created, proof.frozen.clone());
                        plan::set_shapes(&mut created, proof.shapes.clone());
                        plan::set_inputs(&mut created, proof.inputs.clone());
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
                                "refs": s.refs,
                            })).collect::<Vec<_>>(),
                            "budget_limit": created.budget.limit,
                            "baselines": proof.slots,
                            "snapshots": proof.frozen,
                            "shapes": proof.shapes,
                            "inputs": proof.inputs,
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
                                let mut message = format!(
                                    "plan {id} created with {step_count} steps{}",
                                    proof.notes.join("")
                                );
                                // advisory mining: AGENTS.md restricts
                                // something no typed constraint covers.
                                // Silent once the author formalized anything.
                                let typed = created.constraints.iter().any(|text| {
                                    !matches!(
                                        plan::classify_constraint(text),
                                        plan::ConstraintKind::Plain(_)
                                    )
                                });
                                if !typed {
                                    let candidates =
                                        mine_constraint_candidates(&ctx.root);
                                    if !candidates.is_empty() {
                                        message.push_str(&format!(
                                            "\nconstraints: AGENTS.md restricts {} — consider forbid-import:/forbid-cmd:/ast:/path: (advisory; untyped constraints are not enforced)",
                                            candidates.join(" · ")
                                        ));
                                    }
                                }
                                Outcome::ok(message)
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
            match id {
                // No silent whole-plan kill here either: the dispatcher used
                // to abandon directly, bypassing the pure guard below.
                None => Outcome::err(
                    serde_json::json!({
                        "ok": false,
                        "code": "need_step_id",
                        "reason": "cancel needs a step id",
                        "hint": "cancel a step that will not happen, or ask the user to abandon the whole plan (/plan abandon)",
                    })
                    .to_string(),
                ),
                Some(target_id) => {
                    // Whole-plan abandon is the user's call even with the id
                    // spelled out — route it through the pure guard so the
                    // refusal (not a direct status flip) is what lands.
                    if plan::list_active(&ctx.root)
                        .iter()
                        .any(|p| p.id == target_id)
                    {
                        return Outcome::err(
                            serde_json::json!({
                                "ok": false,
                                "code": "abandon_user_only",
                                "reason": format!("plan {target_id} can only be abandoned by the user"),
                                "hint": "surrender a contradiction with block_plan (quote it), or ask the user to abandon it",
                            })
                            .to_string(),
                        );
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
                    // #171: no own plan — but `start` names an explicit step
                    // to work, so resolve the project's active plan and JOIN
                    // it (membership recorded) instead of failing or
                    // silently borrowing foreign work.
                    let is_start = matches!(&other, plan::Op::Start { .. });
                    match (is_start, plan::open_active(&ctx.root)) {
                        (true, Ok(Some(mut foreign))) => {
                            if !foreign.sessions.iter().any(|s| s == &ctx.session_id) {
                                foreign.sessions.push(ctx.session_id.clone());
                                foreign.revision += 1;
                                if let Err(e) = plan::store(&ctx.root, &foreign) {
                                    return Outcome::err(format!("plan join failed: {e:#}"));
                                }
                            }
                            foreign
                        }
                        _ => {
                            return Outcome::err(
                                "no active plan: create one with op=create first".to_string(),
                            );
                        }
                    }
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
            if let plan::Op::Start { ref id, .. } = other
                && let Some(step) = active.step(id)
            {
                let is_initial_start = step.status == plan::StepStatus::Pending;
                if let Err(rej) = validate_plan_refs(&ctx.root, &step.refs, is_initial_start) {
                    return rejection(rej);
                }
            }
            if let plan::Op::Add { ref refs, .. } = other
                && let Err(rej) = validate_plan_refs(&ctx.root, refs, true)
            {
                return rejection(rej);
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
                            ctx.checkpoint_chain(),
                            &format!("step_{id}_start"),
                        )
                    {
                        ctx.journal.push((sha, format!("step_{id}_start")));
                    }
                    if let Some(ref id) = finishing
                        && let Ok(Some(sha)) = crate::agent::checkpoints::snapshot_boundary(
                            &ctx.root,
                            ctx.shadow_store,
                            ctx.checkpoint_chain(),
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

/// How much of a baseline's failing output is kept inline (§12.12). Enough to
/// see *why* it failed — "cannot find function foo" is a baseline, "command
/// not found" is a typo — without copying a test log into the plan file.
const BASELINE_HEAD_CHARS: usize = 600;

/// How much of that reason is repeated in the create result. One line, short
/// enough to read in a collapsed tool row.
const BASELINE_REASON_CHARS: usize = 160;

/// Baselines captured at plan time, plus what to tell the model about the
/// items that did not get one.
pub(crate) struct BaselineProof {
    pub(crate) slots: Vec<Option<plan::Baseline>>,
    /// Rung 4, positional beside `slots`: the frozen outputs of `snapshot:`
    /// items, taken at the same moment under the same host-run rules.
    pub(crate) frozen: Vec<Option<plan::Snapshot>>,
    /// Rung 5, positional beside them: the frozen declaration shapes of
    /// `signatures:` items.
    pub(crate) shapes: Vec<Option<plan::ShapeFreeze>>,
    /// Frozen check inputs, positional: test/fixture hashes for every
    /// `cmd:`/`snapshot:`/`differential:` item. Frozen once, before any
    /// check runs — the only moment the pre-change tree is still current.
    pub(crate) inputs: Vec<Vec<plan::CheckInput>>,
    /// one line per item, prefixed with `\n` so they can be appended raw
    pub(crate) notes: Vec<String>,
}

/// Exit codes that mean the shell never ran the check at all: a typo or a
/// missing tool, not a failure of the code under test.
///
/// Best effort, and only where the shell actually says so. POSIX shells answer
/// 127 (not found) and 126 (not executable). `cmd.exe` and PowerShell answer 1
/// — the same code an ordinary failing check uses — so on Windows a typo *is*
/// recorded as a baseline. That is not a hole: `verify` needs the check to
/// pass, and a typo never passes, so the item simply never settles. The
/// portable guard is the failing output kept beside the baseline and shown to
/// whoever has to judge it.
fn check_never_started(exit: i32) -> bool {
    match crate::agent::shell::ShellKind::detect() {
        crate::agent::shell::ShellKind::Bash | crate::agent::shell::ShellKind::Sh => {
            exit == 126 || exit == 127
        }
        crate::agent::shell::ShellKind::Cmd | crate::agent::shell::ShellKind::PowerShell => false,
    }
}

/// Outcome of freezing one `snapshot:` item: kept, left unfrozen, or the
/// user stopped the turn mid-capture (later items must not start).
enum Frozen {
    Kept(plan::Snapshot),
    Empty,
    Cancelled,
}

/// Rung 4: freeze one `snapshot:` item's output at plan time. The same
/// host-run rules as a baseline — model-controlled text through the
/// classifier, typo guard, no raced runs — but any exit code freezes:
/// erroring the same way is behavior too. An empty output freezes nothing:
/// it discriminates nothing, the same way an already-passing check proves
/// nothing for `cmd:`.
fn freeze_snapshot(
    ctx: &mut ToolCtx,
    paths: &[String],
    index: usize,
    command: &str,
    notes: &mut Vec<String>,
) -> Frozen {
    match safety::classify(command) {
        safety::Verdict::Safe => {}
        safety::Verdict::Blocked(reason) => {
            notes.push(format!(
                "\nacceptance {index}: not run — touches protected path ({reason})"
            ));
            return Frozen::Empty;
        }
        safety::Verdict::NeedsApproval(reason) => {
            notes.push(format!(
                "\nacceptance {index}: not run — would need approval ({reason}); \
                 acceptance commands run unattended, so they must be safe"
            ));
            return Frozen::Empty;
        }
    }
    let state_before = plan::state_digest(&ctx.root, paths, command);
    let run = exec::bash(ctx, command, Some(ACCEPTANCE_TIMEOUT_SECS), false);
    if run.cancelled {
        notes.push(format!("\nacceptance {index}: cancelled"));
        return Frozen::Cancelled;
    }
    let state_after = plan::state_digest(&ctx.root, paths, command);
    let Some(exit) = run.exit_code else {
        notes.push(format!(
            "\nacceptance {index}: could not be run — {}",
            run.output.lines().next().unwrap_or("no result")
        ));
        return Frozen::Empty;
    };
    if check_never_started(exit) {
        notes.push(format!(
            "\nacceptance {index}: could not be run (exit {exit}) — check the command text"
        ));
        return Frozen::Empty;
    }
    if state_before != state_after {
        notes.push(format!(
            "\nacceptance {index}: ran while tracked state moved; nothing frozen"
        ));
        return Frozen::Empty;
    }
    let body: String = run
        .output
        .lines()
        .filter(|line| !line.starts_with("(exit code"))
        .collect::<Vec<_>>()
        .join("\n");
    // `exec` substitutes its own "no output" placeholder for silent runs —
    // that is the runner talking, not the check, so it freezes nothing
    if body.trim().is_empty() || body.trim() == "no output" {
        notes.push(format!(
            "\nacceptance {index}: froze empty output — that discriminates nothing; \
             use cmd: for a pass/fail check"
        ));
        return Frozen::Empty;
    }
    let output_hash = blake3::hash(run.output.as_bytes()).to_hex().to_string();
    let head: String = body.chars().take(BASELINE_HEAD_CHARS).collect();
    let first_line: String = head
        .lines()
        .next()
        .unwrap_or("no output")
        .trim()
        .chars()
        .take(BASELINE_REASON_CHARS)
        .collect();
    notes.push(format!(
        "\nacceptance {index}: frozen (exit {exit}) — {first_line}"
    ));
    Frozen::Kept(plan::Snapshot {
        at: plan::now(),
        exit: Some(exit),
        check_definition_hash: plan::check_definition_hash(command),
        output_hash,
        head,
        state_digest: state_after,
    })
}

/// Rung 3: freeze one `differential:` item's pre-change output. The same
/// host-run rules as [`freeze_snapshot`], plus one of its own: the check
/// runs *twice* and both runs must agree byte for byte. A differential
/// settles on changed output, so a nondeterministic input would pass
/// trivially — the double run proves the input is stable enough to compare.
/// Costs two executions at plan time; that is the rung's price.
fn freeze_differential(
    ctx: &mut ToolCtx,
    paths: &[String],
    index: usize,
    command: &str,
    notes: &mut Vec<String>,
) -> Frozen {
    match safety::classify(command) {
        safety::Verdict::Safe => {}
        safety::Verdict::Blocked(reason) => {
            notes.push(format!(
                "\nacceptance {index}: not run — touches protected path ({reason})"
            ));
            return Frozen::Empty;
        }
        safety::Verdict::NeedsApproval(reason) => {
            notes.push(format!(
                "\nacceptance {index}: not run — would need approval ({reason}); \
                 acceptance commands run unattended, so they must be safe"
            ));
            return Frozen::Empty;
        }
    }
    let state_before = plan::state_digest(&ctx.root, paths, command);
    let first = exec::bash(ctx, command, Some(ACCEPTANCE_TIMEOUT_SECS), false);
    if first.cancelled {
        notes.push(format!("\nacceptance {index}: cancelled"));
        return Frozen::Cancelled;
    }
    let second = exec::bash(ctx, command, Some(ACCEPTANCE_TIMEOUT_SECS), false);
    if second.cancelled {
        notes.push(format!("\nacceptance {index}: cancelled"));
        return Frozen::Cancelled;
    }
    let state_after = plan::state_digest(&ctx.root, paths, command);
    let (Some(exit), Some(second_exit)) = (first.exit_code, second.exit_code) else {
        notes.push(format!("\nacceptance {index}: could not be run"));
        return Frozen::Empty;
    };
    if check_never_started(exit) || check_never_started(second_exit) {
        notes.push(format!(
            "\nacceptance {index}: could not be run (exit {exit}) — check the command text"
        ));
        return Frozen::Empty;
    }
    if state_before != state_after {
        notes.push(format!(
            "\nacceptance {index}: ran while tracked state moved; nothing frozen"
        ));
        return Frozen::Empty;
    }
    // raw outputs carry the "(exit code N)" footer, so this one comparison
    // covers stdout, stderr, and the exit code together
    if first.output != second.output {
        notes.push(format!(
            "\nacceptance {index}: output differs run to run — differential needs \
             a deterministic input; stabilize it or use cmd: for a pass/fail check"
        ));
        return Frozen::Empty;
    }
    let body: String = first
        .output
        .lines()
        .filter(|line| !line.starts_with("(exit code"))
        .collect::<Vec<_>>()
        .join("\n");
    // same vacuity rule as a snapshot: silent output discriminates nothing
    if body.trim().is_empty() || body.trim() == "no output" {
        notes.push(format!(
            "\nacceptance {index}: froze empty output — that discriminates nothing; \
             use cmd: for a pass/fail check"
        ));
        return Frozen::Empty;
    }
    let output_hash = blake3::hash(first.output.as_bytes()).to_hex().to_string();
    let head: String = body.chars().take(BASELINE_HEAD_CHARS).collect();
    let first_line: String = head
        .lines()
        .next()
        .unwrap_or("no output")
        .trim()
        .chars()
        .take(BASELINE_REASON_CHARS)
        .collect();
    notes.push(format!(
        "\nacceptance {index}: frozen differential, stable across 2 runs (exit {exit}) — {first_line}"
    ));
    Frozen::Kept(plan::Snapshot {
        at: plan::now(),
        exit: Some(exit),
        check_definition_hash: plan::check_definition_hash(command),
        output_hash,
        head,
        state_digest: state_after,
    })
}

/// Outcome of freezing one `signatures:` item. No cancellation arm:
/// reading files is fast and has no user-cancel point, unlike executions.
enum Shaped {
    Kept(plan::ShapeFreeze),
    Empty,
}

/// Files above this are not shape-read: declaration outlines are for
/// source, not dumps. Mirrors the `outline` tool's ceiling.
const SHAPE_MAX_BYTES: u64 = 512_000;

/// Rung 5: freeze the declaration shapes of the named files. No commands
/// run — the host only reads — so there is no safety classification and
/// no typo guard; the failure modes are missing/unreadable files instead.
/// All-or-nothing: one bad path leaves the whole item unfrozen with a note
/// naming it, so a typo cannot silently narrow the commitment.
fn freeze_shapes(ctx: &mut ToolCtx, index: usize, paths: &str, notes: &mut Vec<String>) -> Shaped {
    let names = plan::signature_paths(paths);
    if names.is_empty() {
        notes.push(format!(
            "\nacceptance {index}: names no files — a signatures item without paths settles nothing"
        ));
        return Shaped::Empty;
    }
    let mut files = Vec::with_capacity(names.len());
    let mut total_items = 0usize;
    let mut summary = Vec::with_capacity(names.len());
    for name in &names {
        let full = match ctx.resolve(name) {
            Ok(full) => full,
            Err(message) => {
                notes.push(format!("\nacceptance {index}: {message}"));
                return Shaped::Empty;
            }
        };
        let meta = match std::fs::metadata(&full) {
            Ok(meta) => meta,
            Err(_) => {
                notes.push(format!(
                    "\nacceptance {index}: '{name}' is missing — freeze the files before the work starts"
                ));
                return Shaped::Empty;
            }
        };
        if meta.is_dir() {
            notes.push(format!(
                "\nacceptance {index}: '{name}' is a directory — name source files"
            ));
            return Shaped::Empty;
        }
        if meta.len() > SHAPE_MAX_BYTES {
            notes.push(format!(
                "\nacceptance {index}: '{name}' is too large to shape-read"
            ));
            return Shaped::Empty;
        }
        let bytes = match std::fs::read(&full) {
            Ok(bytes) => bytes,
            Err(e) => {
                notes.push(format!("\nacceptance {index}: cannot read '{name}': {e}"));
                return Shaped::Empty;
            }
        };
        let src = match std::str::from_utf8(&bytes) {
            Ok(src) => src,
            Err(_) => {
                notes.push(format!(
                    "\nacceptance {index}: '{name}' is binary or not valid UTF-8"
                ));
                return Shaped::Empty;
            }
        };
        let ext = full.extension().and_then(|e| e.to_str()).unwrap_or("");
        let (parser, shape) = crate::agent::tools::outline::shape_of(src, ext);
        let shape_hash = blake3::hash(shape.join("\n").as_bytes()).to_hex().to_string();
        total_items += shape.len();
        summary.push(format!("{name} ({parser}, {})", shape.len()));
        files.push(plan::ShapeFile {
            path: name.to_string(),
            parser,
            shape_hash,
            items: shape.len(),
        });
    }
    notes.push(format!(
        "\nacceptance {index}: froze {} file(s), {total_items} declaration(s) — {}",
        files.len(),
        summary.join(", ")
    ));
    Shaped::Kept(plan::ShapeFreeze {
        at: plan::now(),
        check_definition_hash: plan::check_definition_hash(paths),
        files,
    })
}

/// Re-read the shapes named by a frozen rung-5 record: `(path, hash)` per
/// file, `None` where the file no longer reads. A deleted file is a
/// changed shape, not an error — removal breaks a freeze like any edit.
fn read_shapes(ctx: &ToolCtx, files: &[plan::ShapeFile]) -> Vec<(String, Option<String>)> {
    files
        .iter()
        .map(|file| {
            let hash = ctx.resolve(&file.path).ok().and_then(|full| {
                std::fs::read(&full).ok().and_then(|bytes| {
                    std::str::from_utf8(&bytes).ok().map(|src| {
                        let ext = full.extension().and_then(|e| e.to_str()).unwrap_or("");
                        let (_, shape) = crate::agent::tools::outline::shape_of(src, ext);
                        blake3::hash(shape.join("\n").as_bytes()).to_hex().to_string()
                    })
                })
            });
            (file.path.clone(), hash)
        })
        .collect()
}

/// §12.12: run every `cmd:` acceptance item once and keep the runs that
/// failed. Called at plan creation — the only moment the pre-change tree is
/// still the current one. Once the work starts there is nothing left to prove
/// that the check can fail at all, and a check that cannot fail settles
/// nothing.
///
/// Nothing here is fatal. A check that already passes, a command that cannot
/// run, an unsafe command: each just leaves its item without a baseline. The
/// item is shown in that state and can never be settled from it; the refusal
/// happens at `plan verify`, where the model can do something about it.
pub(crate) fn capture_baselines(ctx: &mut ToolCtx, plan: &plan::Plan) -> BaselineProof {
    // pre-sized: every item already has its three slots, so each branch
    // assigns by index and a cancelled capture just stops.
    let mut proof = BaselineProof {
        slots: vec![None; plan.acceptance.len()],
        frozen: vec![None; plan.acceptance.len()],
        shapes: vec![None; plan.acceptance.len()],
        inputs: vec![Vec::new(); plan.acceptance.len()],
        notes: Vec::new(),
    };
    // check inputs freeze once, before any check runs: hashing is
    // read-only, identical for every item, and the tree is pre-change
    // only at this moment.
    let frozen_inputs = plan::freeze_check_inputs(&ctx.root);
    let paths = plan::digest_paths(plan);
    for (index, item) in plan.acceptance.iter().enumerate() {
        if matches!(
            item.kind(),
            plan::AcceptanceKind::Command(_)
                | plan::AcceptanceKind::Snapshot(_)
                | plan::AcceptanceKind::Differential(_)
        ) {
            proof.inputs[index] = frozen_inputs.clone();
        }
        // rungs 3 and 4 freeze beside the baselines: the same moment, the
        // same host-run rules, the same positional slots. Only the verdict
        // is inverted — and rung 3 proves determinism with a double run.
        let frozen_command = match item.kind() {
            plan::AcceptanceKind::Snapshot(command) => Some((false, command.to_string())),
            plan::AcceptanceKind::Differential(command) => Some((true, command.to_string())),
            _ => None,
        };
        if let Some((is_differential, command)) = frozen_command {
            let frozen = if is_differential {
                freeze_differential(ctx, &paths, index, &command, &mut proof.notes)
            } else {
                freeze_snapshot(ctx, &paths, index, &command, &mut proof.notes)
            };
            match frozen {
                Frozen::Kept(snapshot) => proof.frozen[index] = Some(snapshot),
                Frozen::Empty => {}
                // the user is stopping the turn; later items keep their
                // pre-sized empty slots
                Frozen::Cancelled => break,
            }
            continue;
        }
        // rung 5 freezes declaration shapes instead of command output.
        if let plan::AcceptanceKind::Signatures(paths) = item.kind() {
            let paths = paths.join(", ");
            match freeze_shapes(ctx, index, &paths, &mut proof.notes) {
                Shaped::Kept(shape) => proof.shapes[index] = Some(shape),
                Shaped::Empty => {}
            }
            continue;
        }
        let plan::AcceptanceKind::Command(command) = item.kind() else {
            continue;
        };
        let command = command.to_string();
        // The command text arrives from the model and is about to be run
        // without asking, so anything that would need approval is skipped
        // rather than run — the same refusal `plan verify` makes, moved to
        // where the model can still rewrite the item.
        match safety::classify(&command) {
            safety::Verdict::Safe => {}
            safety::Verdict::Blocked(reason) => {
                proof.notes.push(format!(
                    "\nacceptance {index}: not run — touches protected path ({reason})"
                ));
                continue;
            }
            safety::Verdict::NeedsApproval(reason) => {
                proof.notes.push(format!(
                    "\nacceptance {index}: not run — would need approval ({reason}); \
                     acceptance commands run unattended, so they must be safe"
                ));
                continue;
            }
        }
        let state_before = plan::state_digest(&ctx.root, &paths, &command);
        let run = exec::bash(ctx, &command, Some(ACCEPTANCE_TIMEOUT_SECS), false);
        if run.cancelled {
            proof.notes.push(format!("\nacceptance {index}: cancelled"));
            // the user is stopping the turn; later items keep their
            // pre-sized empty slots
            break;
        }
        let state_after = plan::state_digest(&ctx.root, &paths, &command);
        let Some(exit) = run.exit_code else {
            proof.notes.push(format!(
                "\nacceptance {index}: could not be run — {}",
                run.output.lines().next().unwrap_or("no result")
            ));
            continue;
        };
        if exit == 0 {
            proof.notes.push(format!(
                "\nacceptance {index}: passes already — that makes it a regression \
                 guard, not acceptance, and it will never settle this item"
            ));
            continue;
        }
        // The shell saying the check never started, rather than the check
        // failing. Recording one of those as a baseline would make the proof
        // meaningless, so a typo stays a typo instead of becoming evidence.
        if check_never_started(exit) {
            proof.notes.push(format!(
                "\nacceptance {index}: could not be run (exit {exit}) — check the command text"
            ));
            continue;
        }
        if state_before != state_after {
            proof.notes.push(format!(
                "\nacceptance {index}: ran while tracked state moved; no baseline taken"
            ));
            continue;
        }
        let output_hash = blake3::hash(run.output.as_bytes()).to_hex().to_string();
        let head: String = run
            .output
            .lines()
            .filter(|line| !line.starts_with("(exit code"))
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .take(BASELINE_HEAD_CHARS)
            .collect();
        // The reason travels with the verdict: on a shell that reports a typo
        // as an ordinary failure, this line is the only thing that separates
        // "the feature is missing" from "the command does not exist".
        let first_line: String = head
            .lines()
            .next()
            .unwrap_or("no output")
            .trim()
            .chars()
            .take(BASELINE_REASON_CHARS)
            .collect();
        proof.slots[index] = Some(plan::Baseline {
            at: plan::now(),
            exit,
            check_definition_hash: plan::check_definition_hash(&command),
            output_hash,
            head,
            state_digest: state_after,
        });
        proof.notes.push(format!(
            "\nacceptance {index}: fails before the change (exit {exit}) — {first_line}"
        ));
    }
    // ladder walk (§12.12): one trailing line saying where on the ladder
    // this plan stands. Rides every create/accept message for free, since
    // both append these notes raw.
    proof.notes.push(plan::ladder_note(plan));
    proof
}

/// §12.12 three states: same check, prior green receipt, same digest,
/// opposite outcome — the runs disagree, so the item is flaky rather than
/// failed. A red run on moved state is an ordinary regression
/// (`acceptance_failed`); only an attested state that now answers
/// differently proves the check itself untrustworthy.
///
/// The digest is the host's whole visibility: a change it cannot see (an
/// untracked file) reads as a flake, honestly — the host truly cannot tell
/// those apart. Tracking what the check depends on in step refs is what
/// keeps genuine regressions out of this verdict.
fn same_state_disagreement(item: &plan::Acceptance, command: &str, state_digest: &str) -> bool {
    if item.status != plan::AcceptanceStatus::Passed {
        return false;
    }
    let hash = plan::check_definition_hash(command);
    item.validation
        .receipts
        .iter()
        .rev()
        .find(|receipt| receipt.check_definition_hash.as_deref() == Some(hash.as_str()))
        .and_then(|receipt| receipt.state_after.as_deref())
        == Some(state_digest)
}

/// Receipt-time check-inputs verdict, shared by every host-run acceptance
/// path (`cmd:`, `snapshot:`, `differential:`, at `verify` and at
/// `complete`). Re-hashes the inputs frozen at plan time: a changed or
/// vanished file means the check no longer runs against what was frozen —
/// usually edited tests — so no verdict may issue. Records the event in
/// the journal and returns the rejection message; `None` means clean.
/// Waived items skip: the user took responsibility with the waiver.
fn inputs_verdict(
    ctx: &mut ToolCtx,
    item: &plan::Acceptance,
    index: usize,
) -> Result<Option<String>, String> {
    if item.status == plan::AcceptanceStatus::Waived {
        return Ok(None);
    }
    let changed = plan::changed_check_inputs(&ctx.root, &item.inputs);
    if changed.is_empty() {
        return Ok(None);
    }
    let text = format!(
        "acceptance {index} check inputs changed since plan time: {}",
        changed.join(", ")
    );
    match crate::agent::journal::Journal::open(&ctx.root, &ctx.session_id) {
        Ok(mut journal) => {
            let _ = journal.append(
                "note",
                serde_json::json!({
                    "by": "host",
                    "note": "blocker",
                    "text": text,
                }),
            );
        }
        Err(e) => return Err(format!("receipt journal unwritable: {e:#}")),
    }
    Ok(Some(format!(
        "inputs_changed: {text} — restore the frozen files, or have the user \
         waive the item with /plan waive"
    )))
}

/// Refusal for a flaky item (§12.12): reported as such, never silently
/// retried into verified. Waiver is the way out.
fn flaky_rejection(index: usize, command: &str) -> plan::Rejection {    plan::Rejection {
        code: "flaky_check",
        reason: format!("acceptance {index} runs disagree on the same state: {command}"),
        hint: "a green run and a red run attested the same digest, so the check \
               is flaky rather than failed: make it deterministic, or have the user \
               waive the item with /plan waive"
            .to_string(),
    }
}

/// Interval receipt for a host-run check (§2.1.4): equal before/after
/// digests, exec runner, and a matching journal record. Shared by the
/// `cmd:` and `snapshot:` verify paths so both runners attest identically.
#[allow(clippy::too_many_arguments)]
fn issue_exec_receipt(
    ctx: &mut ToolCtx,
    index: usize,
    command: &str,
    runner: &str,
    started_at: String,
    finished_at: String,
    state_before: String,
    state_after: String,
    paths: Vec<String>,
    exit_code: Option<i32>,
    output_hash: String,
) -> Result<plan::Receipt, String> {
    let receipt_fields = serde_json::json!({
        "check_definition_hash": plan::check_definition_hash(command),
        "runner": runner,
        "command": command,
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
                    return Err(format!("receipt journal unwritable: {e:#}"));
                }
            }
        }
        Err(e) => {
            return Err(format!("receipt journal unwritable: {e:#}"));
        }
    };
    Ok(plan::Receipt {
        session: ctx.session_id.clone(),
        seq,
        state_digest: state_after.clone(),
        command: Some(command.to_string()),
        exit: exit_code,
        at: finished_at.clone(),
        check_definition_hash: Some(plan::check_definition_hash(command)),
        runner: Some(runner.to_string()),
        args: None,
        cwd: Some(ctx.root.display().to_string()),
        started_at: Some(started_at),
        finished_at: Some(finished_at),
        state_before: Some(state_before),
        state_after: Some(state_after),
        output_hash: Some(output_hash),
        paths,
    })
}

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

    // §12.12 three states: a flaky item is reported, never silently retried
    // into verified — no run is spent here, whatever the item's kind.
    if item.validation.status == plan::ValidationStatus::Unknown {
        return rejection(flaky_rejection(index, &item.text));
    }

    let (evidence, receipt) = match item.kind() {
        plan::AcceptanceKind::Manual(_) => (Vec::new(), None),
        plan::AcceptanceKind::Command(command) => {
            let command = command.to_string();
            // §12.12: a green run only means something if red was possible.
            // This is a rule about what the host may *accept*, so it sits here
            // rather than in `plan::verify_acceptance`, which replay also
            // drives — and replay restores commits that were already accepted,
            // so it must not re-judge them.
            if !plan::proven_failing(item) {
                let hint = match item.baseline.as_ref() {
                    Some(baseline) => format!(
                        "the check was rewritten after its baseline was taken \
                         (failed with exit {} at {}); prove the new text fails on \
                         the pre-change state, or have the user waive the item \
                         with /plan waive",
                        baseline.exit, baseline.at
                    ),
                    None => "no run of this check has ever failed here, so passing \
                             it proves nothing: make it reproduce the problem before \
                             the work starts, or have the user waive the item with \
                             /plan waive"
                        .to_string(),
                };
                return rejection(plan::Rejection {
                    code: "no_baseline",
                    reason: format!(
                        "acceptance {index} has no proof that it can fail: {command}"
                    ),
                    hint,
                });
            }
            // frozen check inputs first: a verdict on edited tests settles
            // nothing, whichever way the run goes.
            match inputs_verdict(ctx, item, index) {
                Err(message) => return Outcome::err(message),
                Ok(Some(message)) => return Outcome::err(message),
                Ok(None) => {}
            }
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
                let state_after = plan::state_digest(&ctx.root, &paths, &command);
                // same attested state, opposite outcome: flaky, not failed
                if state_before == state_after
                    && same_state_disagreement(&active.acceptance[index], &command, &state_after)
                {
                    if plan::apply_flaky(&mut active, index) {
                        let args = serde_json::json!({
                            "index": index,
                            "command": command,
                            "state_digest": state_after,
                        });
                        if let Err(e) = plan::commit(
                            &ctx.root,
                            &ctx.session_id,
                            &mut active,
                            "flaky",
                            "host",
                            true,
                            args,
                        ) {
                            return Outcome::err(format!("plan write failed: {e:#}"));
                        }
                    }
                    return rejection(flaky_rejection(index, &command));
                }
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
            let receipt = match issue_exec_receipt(
                ctx,
                index,
                &command,
                "exec",
                started_at,
                finished_at,
                state_before,
                state_after,
                paths,
                exit_code,
                output_hash,
            ) {
                Ok(receipt) => receipt,
                Err(message) => return Outcome::err(message),
            };
            (Vec::new(), Some(receipt))
        }
        plan::AcceptanceKind::Snapshot(command) => {
            let command = command.to_string();
            // rung 4 settles on frozen behavior, not on failure: the host
            // must have frozen this exact check at plan time. Like the
            // baseline gate this lives at the host boundary so replay of
            // accepted commits never re-judges them.
            if !plan::snapshot_current(item) {
                let hint = match item.snapshot.as_ref() {
                    Some(frozen) => format!(
                        "the check was rewritten after its output was frozen \
                         (at {}); freeze the new text on the pre-change state, \
                         or have the user waive the item with /plan waive",
                        frozen.at
                    ),
                    None => "no run of this check was ever frozen here, so no output \
                             can match it: freeze the behavior before the work \
                             starts, or have the user waive the item with \
                             /plan waive"
                        .to_string(),
                };
                return rejection(plan::Rejection {
                    code: "no_snapshot",
                    reason: format!(
                        "acceptance {index} has no frozen output to compare to: {command}"
                    ),
                    hint,
                });
            }
            match inputs_verdict(ctx, item, index) {
                Err(message) => return Outcome::err(message),
                Ok(Some(message)) => return Outcome::err(message),
                Ok(None) => {}
            }
            let frozen = item.snapshot.as_ref().expect("gated above");
            // model-controlled input, same classifier as `bash`: frozen or
            // not, an unsafe check never runs unattended.
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
            let paths = plan::digest_paths(&active);
            let state_before = plan::state_digest(&ctx.root, &paths, &command);
            let started_at = plan::now();
            let run = exec::bash(ctx, &command, Some(ACCEPTANCE_TIMEOUT_SECS), false);
            let finished_at = plan::now();
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
            if output_hash == frozen.output_hash && run.exit_code == frozen.exit {
                let receipt = match issue_exec_receipt(
                    ctx,
                    index,
                    &command,
                    "exec",
                    started_at,
                    finished_at,
                    state_before,
                    state_after,
                    paths,
                    run.exit_code,
                    output_hash,
                ) {
                    Ok(receipt) => receipt,
                    Err(message) => return Outcome::err(message),
                };
                (Vec::new(), Some(receipt))
            } else {
                // the output moved: nondeterministic on the same state is a
                // flake, changed on moved state is changed behavior
                if same_state_disagreement(&active.acceptance[index], &command, &state_after) {
                    if plan::apply_flaky(&mut active, index) {
                        let args = serde_json::json!({
                            "index": index,
                            "command": command,
                            "state_digest": state_after,
                        });
                        if let Err(e) = plan::commit(
                            &ctx.root,
                            &ctx.session_id,
                            &mut active,
                            "flaky",
                            "host",
                            true,
                            args,
                        ) {
                            return Outcome::err(format!("plan write failed: {e:#}"));
                        }
                    }
                    return rejection(flaky_rejection(index, &command));
                }
                return rejection(plan::Rejection {
                    code: "snapshot_changed",
                    reason: format!("acceptance {index} output no longer matches: {command}"),
                    hint: format!(
                        "frozen at {} (exit {:?}); now exit {:?} — {}",
                        frozen.at,
                        frozen.exit,
                        run.exit_code,
                        run.output.lines().take(6).collect::<Vec<_>>().join(" / ")
                    ),
                });
            }
        }
        plan::AcceptanceKind::Differential(command) => {
            let command = command.to_string();
            // rung 3 settles on *changed* output against the same frozen
            // record rung 4 compares for equality. The freeze gate is the
            // same shape with the inverted meaning.
            if !plan::differential_current(item) {
                let hint = match item.snapshot.as_ref() {
                    Some(frozen) => format!(
                        "the check was rewritten after its output was frozen \
                         (at {}); freeze the new text on the pre-change state, \
                         or have the user waive the item with /plan waive",
                        frozen.at
                    ),
                    None => "no run of this check was ever frozen here, so no output \
                             can be compared: freeze the behavior before the work \
                             starts, or have the user waive the item with \
                             /plan waive"
                        .to_string(),
                };
                return rejection(plan::Rejection {
                    code: "no_snapshot",
                    reason: format!(
                        "acceptance {index} has no frozen output to compare to: {command}"
                    ),
                    hint,
                });
            }
            match inputs_verdict(ctx, item, index) {
                Err(message) => return Outcome::err(message),
                Ok(Some(message)) => return Outcome::err(message),
                Ok(None) => {}
            }
            let frozen = item.snapshot.as_ref().expect("gated above");
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
            let paths = plan::digest_paths(&active);
            let state_before = plan::state_digest(&ctx.root, &paths, &command);
            let started_at = plan::now();
            let run = exec::bash(ctx, &command, Some(ACCEPTANCE_TIMEOUT_SECS), false);
            let finished_at = plan::now();
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
            if output_hash == frozen.output_hash && run.exit_code == frozen.exit {
                return rejection(plan::Rejection {
                    code: "no_observable_change",
                    reason: format!(
                        "acceptance {index} output identical to the pre-change run: {command}"
                    ),
                    hint: "the change has no observable effect through this input — pick \
                           an input the work actually moves, or use snapshot: if the \
                           behavior must not move"
                        .to_string(),
                });
            }
            // changed output settles only a working change: a command that
            // now exits non-zero broke the input, it did not move it.
            if run.exit_code != Some(0) {
                return rejection(plan::Rejection {
                    code: "broken_change",
                    reason: format!(
                        "acceptance {index} output changed but exits {:?}: {command}",
                        run.exit_code
                    ),
                    hint: "differential settles behavior that changed and works — fix \
                           the breakage, or have the user waive the item with \
                           /plan waive"
                        .to_string(),
                });
            }
            // observably changed through this input, and working: the
            // work moved the behavior without breaking the check
            let receipt = match issue_exec_receipt(
                ctx,
                index,
                &command,
                "exec",
                started_at,
                finished_at,
                state_before,
                state_after,
                paths,
                run.exit_code,
                output_hash,
            ) {
                Ok(receipt) => receipt,
                Err(message) => return Outcome::err(message),
            };
            (Vec::new(), Some(receipt))
        }
        plan::AcceptanceKind::Signatures(paths) => {
            let canonical = paths.join(", ");
            // rung 5 settles on frozen declaration shapes: nothing was
            // executed, so there is no baseline to demand — only the freeze.
            // Like the output gates this lives at the host boundary so
            // replay never re-judges accepted commits.
            if !plan::signatures_current(item) {
                let hint = match item.shape.as_ref() {
                    Some(frozen) => format!(
                        "the file set was renamed after its shapes were frozen \
                         (at {}); freeze the new set on the pre-change tree, \
                         or have the user waive the item with /plan waive",
                        frozen.at
                    ),
                    None => "no shapes were ever frozen here, so nothing can be \
                             compared: freeze the files before the work starts, \
                             or have the user waive the item with /plan waive"
                        .to_string(),
                };
                return rejection(plan::Rejection {
                    code: "no_signatures",
                    reason: format!(
                        "acceptance {index} has no frozen shapes to compare to: {canonical}"
                    ),
                    hint,
                });
            }
            let frozen = item.shape.as_ref().expect("gated above");
            // the digest covers the named files plus the plan refs, so a
            // concurrent edit cannot slip between the re-reads unnoticed
            let mut shape_paths = plan::digest_paths(&active);
            for file in &frozen.files {
                if !shape_paths.contains(&file.path) {
                    shape_paths.push(file.path.clone());
                }
            }
            let state_before = plan::state_digest(&ctx.root, &shape_paths, &canonical);
            let started_at = plan::now();
            let current = read_shapes(ctx, &frozen.files);
            let finished_at = plan::now();
            let state_after = plan::state_digest(&ctx.root, &shape_paths, &canonical);
            if state_before != state_after {
                return rejection(plan::Rejection {
                    code: "state_changed_during_check",
                    reason: format!(
                        "acceptance {index} ran while tracked state moved; no receipt issued"
                    ),
                    hint: "run verify again on the settled state".to_string(),
                });
            }
            let changed: Vec<&str> = frozen
                .files
                .iter()
                .zip(current.iter())
                .filter(|(frozen_file, (_, hash))| {
                    hash.as_deref() != Some(frozen_file.shape_hash.as_str())
                })
                .map(|(frozen_file, _)| frozen_file.path.as_str())
                .collect();
            if changed.is_empty() {
                let output_hash = blake3::hash(
                    current
                        .iter()
                        .map(|(_, hash)| hash.as_deref().unwrap_or(""))
                        .collect::<Vec<_>>()
                        .join("\n")
                        .as_bytes(),
                )
                .to_hex()
                .to_string();
                let receipt = match issue_exec_receipt(
                    ctx,
                    index,
                    &canonical,
                    "ast",
                    started_at,
                    finished_at,
                    state_before,
                    state_after,
                    shape_paths,
                    None,
                    output_hash,
                ) {
                    Ok(receipt) => receipt,
                    Err(message) => return Outcome::err(message),
                };
                (Vec::new(), Some(receipt))
            } else {
                return rejection(plan::Rejection {
                    code: "signatures_changed",
                    reason: format!(
                        "acceptance {index} declaration shapes changed: {}",
                        changed.join(", ")
                    ),
                    hint: "restore the declared structure, or have the user waive \
                           the item with /plan waive"
                        .to_string(),
                });
            }
        }
        plan::AcceptanceKind::Text(text) => {
            // Untyped acceptance cannot be created anymore; files written
            // before the gate still load, but nothing settles them. The
            // pure layer repeats this verdict for replayed commits.
            return rejection(plan::Rejection {
                code: "untyped_acceptance",
                reason: format!("acceptance {index} is free text: {text}"),
                hint: "rewrite it as cmd: (pass/fail), snapshot: (frozen output), \
                       differential: (changed output), signatures: (file shapes), \
                       or manual: (the user checks it by hand)"
                    .to_string(),
            });
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
        crate::agent::journal::Journal::open_assumptions_in(&ctx.root, &ctx.session_id, Some(step))
            .unwrap_or_default();
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

/// The gate on `plan finish`.
///
/// Soft steps (the default) close on a summary alone: the pure finish
/// validator already demands it, and progress is read from receipts, not
/// from gates. Strict mode additionally demands host-recorded evidence of
/// successful work on the step — the same content rule acceptance
/// evidence follows, so a research step can no longer close on a bare
/// failed call, and no step closes on errored diagnostics.
fn validate_evidence(
    root: &Path,
    op: &plan::Op,
    session_id: Option<&str>,
    strict: bool,
) -> Result<(), String> {
    let plan::Op::Finish { id, .. } = op else {
        return Ok(());
    };
    if !strict {
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
    let evidence = active
        .step(id)
        .map(|step| step.evidence.clone())
        .unwrap_or_default();
    validate_attached_records(root, &active.id, id, &evidence)
}

/// Source extensions scanned for `forbid-import:` violations. Mirrors
/// the outline/ast-grep language set.
const CONSTRAINT_SOURCE_EXTS: &[&str] = &[
    "rs", "py", "js", "mjs", "cjs", "jsx", "ts", "mts", "cts", "tsx", "go", "sh", "bash", "c",
    "h", "cpp", "cc", "cxx", "hpp", "hh", "hxx", "cs", "java",
];

/// Never scanned for constraint evaluation: VCS, host state, build outputs.
const CONSTRAINT_SKIP_DIRS: &[&str] = &[".git", ".sqwai", "target", "node_modules", "dist", "build"];

/// Caps mirror the `ast_grep` tool's own ceilings.
const CONSTRAINT_MAX_FILES: usize = 2_000;
const CONSTRAINT_MAX_BYTES: u64 = 512_000;
const CONSTRAINT_MAX_HITS: usize = 5;

/// Import-statement keywords across the scanned languages. A line counts
/// when it opens with one of these (after whitespace) and names the
/// pattern — plus bare quoted references (`"x/y"` import-block entries),
/// minus comment lines.
fn import_line_references(line: &str, pattern: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "use ",
        "use\t",
        "import ",
        "import\t",
        "from ",
        "require",
        "include ",
        "mod ",
        "extern crate ",
    ];
    let trimmed = line.trim_start();
    if crate::agent::lint::is_comment(line) {
        return false;
    }
    if KEYWORDS.iter().any(|kw| trimmed.starts_with(kw)) {
        return trimmed.contains(pattern);
    }
    let bare = trimmed.trim_end_matches([',', ';']);
    bare.len() > 2
        && (bare.starts_with('"') && bare.ends_with('"')
            || bare.starts_with('\'') && bare.ends_with('\''))
        && bare.contains(pattern)
}

/// `forbid-import:` violations as `path:line: text`, capped. Heuristic by
/// design (Go block imports without keywords only match as bare quoted
/// strings); the waiver covers false positives, silence would cover
/// violations.
fn forbid_import_violations(root: &Path, pattern: &str) -> Vec<String> {
    fn walk(
        root: &Path,
        dir: &Path,
        pattern: &str,
        out: &mut Vec<String>,
        scanned: &mut usize,
    ) {
        if out.len() >= CONSTRAINT_MAX_HITS || *scanned >= CONSTRAINT_MAX_FILES {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if out.len() >= CONSTRAINT_MAX_HITS || *scanned >= CONSTRAINT_MAX_FILES {
                return;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if !CONSTRAINT_SKIP_DIRS.contains(&name.as_str()) {
                    walk(root, &path, pattern, out, scanned);
                }
                continue;
            }
            let ext_ok = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|ext| CONSTRAINT_SOURCE_EXTS.contains(&ext));
            if !ext_ok {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if meta.len() > CONSTRAINT_MAX_BYTES {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            *scanned += 1;
            let rel = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| path.to_string_lossy().to_string());
            for (n, line) in text.lines().enumerate() {
                if import_line_references(line, pattern) {
                    out.push(format!("{}:{}: {}", rel, n + 1, line.trim()));
                    if out.len() >= CONSTRAINT_MAX_HITS {
                        return;
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    let mut scanned = 0usize;
    walk(root, root, pattern, &mut out, &mut scanned);
    out
}

/// File paths this plan's steps recorded as written: journal `file_diff`
/// records behind the steps' evidence refs. The same host-recorded source
/// the scope guard reads — bash-written bytes stay invisible here too.
fn plan_file_diff_paths(root: &Path, plan: &plan::Plan) -> Vec<String> {
    let mut out = Vec::new();
    for step in &plan.steps {
        for reference in &step.evidence {
            let record = match crate::agent::journal::Journal::evidence(
                root,
                &plan.id,
                Some(&step.id),
                reference,
                None,
            ) {
                Ok(Some(record)) => record,
                _ => continue,
            };
            if record.kind == "file_diff"
                && let Some(path) = record.fields.get("path").and_then(Value::as_str)
            {
                let clean = lexical_clean(&path.replace('\\', "/"));
                if !out.contains(&clean) {
                    out.push(clean);
                }
            }
        }
    }
    out.sort();
    out
}

/// Pure half of the `forbid-cmd:` live gate: first pattern matching the
/// command (case-insensitive substring), if any. Heuristic like every
/// text match; the waiver covers overmatches.
pub(crate) fn forbidden_command(patterns: &[String], command: &str) -> Option<String> {
    let lower = command.to_lowercase();
    patterns
        .iter()
        .filter(|p| !p.trim().is_empty())
        .find(|p| lower.contains(&p.to_lowercase()))
        .cloned()
}

/// Typed-constraint verdicts at `complete`: every non-waived executable
/// constraint must hold. `forbid-cmd:` is live-gated in `bash_call` and
/// has nothing to re-check; unprefixed constraints are advisory.
fn validate_constraints(ctx: &mut ToolCtx, active: &plan::Plan) -> Result<(), String> {
    let waived = plan::waived_constraint_indices(active);
    for (index, text) in active.constraints.iter().enumerate() {
        if waived.contains(&index) {
            continue;
        }
        let failed: Option<String> = match plan::classify_constraint(text) {
            plan::ConstraintKind::Plain(_) => None,
            plan::ConstraintKind::ForbidCmd(_) => None,
            plan::ConstraintKind::ForbidImport(pattern) => {
                if pattern.trim().is_empty() {
                    continue;
                }
                let hits = forbid_import_violations(&ctx.root, pattern);
                (!hits.is_empty()).then(|| {
                    format!("forbidden import '{pattern}' referenced at {}", hits.join("; "))
                })
            }
            plan::ConstraintKind::Ast(pattern) => {
                if pattern.trim().is_empty() {
                    continue;
                }
                let outcome = astgrep::ast_grep(
                    ctx,
                    &serde_json::json!({"pattern": pattern, "path": ".", "max": 3}),
                );
                if !outcome.ok {
                    Some(format!("ast pattern failed: {}", outcome.output))
                } else if outcome.output.starts_with("0 matches") {
                    None
                } else {
                    let head: Vec<&str> = outcome.output.lines().skip(1).take(3).collect();
                    Some(format!(
                        "ast pattern `{pattern}` matches: {}",
                        head.join(" / ")
                    ))
                }
            }
            plan::ConstraintKind::Path(roots) => {
                if roots.is_empty() {
                    continue;
                }
                let roots: Vec<String> = roots
                    .iter()
                    .map(|r| lexical_clean(&r.replace('\\', "/")))
                    .collect();
                let outside: Vec<String> = plan_file_diff_paths(&ctx.root, active)
                    .into_iter()
                    .filter(|path| !in_write_scope(path, &roots))
                    .collect();
                (!outside.is_empty()).then(|| {
                    format!(
                        "change escapes the declared roots ({}): {}",
                        roots.join(", "),
                        outside.join(", ")
                    )
                })
            }
        };
        if let Some(details) = failed {
            return Err(format!(
                "constraint_violated: constraint {index} violated: {details} — fix it, or have the user waive it (/plan waive-constraint {index} <reason>)"
            ));
        }
    }
    Ok(())
}

/// The gate on `plan complete`.///
/// Every done step is re-checked against the journal, and every acceptance
/// item is settled again on its own terms: a `cmd:` item is **re-run** rather
/// than trusted from an earlier verify (§2.1.2 has the host run it "on `plan
/// verify` and on `complete`"; §2.4.11 makes the same point for the full test
/// suite), and a `Text` item is re-checked against the records it was verified
/// with. A waived item is the user's call and is left alone.
fn validate_complete(ctx: &mut ToolCtx) -> Result<(), String> {
    let root = ctx.root.clone();
    let mut active = plan::open_active_for_session(&root, Some(&ctx.session_id))
        .map_err(|e| format!("evidence_unreadable: {e:#}"))?
        .ok_or_else(|| "invalid_evidence: no active plan".to_string())?;
    for step in active
        .steps
        .iter()
        .filter(|step| step.status == plan::StepStatus::Done)
    {
        // soft steps may close on a summary alone: nothing recorded means
        // nothing to re-check. Strict mode never gets here without evidence,
        // because its finish gate already demanded it.
        if step.evidence.is_empty() {
            continue;
        }
        validate_attached_records(&root, &active.id, &step.id, &step.evidence)?;
    }
    // typed constraints settle like acceptance: a violated executable
    // constraint blocks completion the same way a red check does.
    validate_constraints(ctx, &active)?;
    // by index: a flaky verdict below mutates the plan, which an
    // iterator borrow would not allow
    for index in 0..active.acceptance.len() {
        if active.acceptance[index].status != plan::AcceptanceStatus::Passed {
            continue;
        }
        // state moved under a recorded check after verification: the pure
        // complete gate rejects this too, but failing here names the fix
        // (re-verify) before the op is even attempted
        if active.acceptance[index].validation.status == plan::ValidationStatus::Stale {
            return Err(format!(
                "acceptance_stale: acceptance {index} went stale after verification (tracked files changed); re-verify it, then complete"
            ));
        }
        match active.acceptance[index].kind() {
            plan::AcceptanceKind::Command(command) => {
                if let Some(message) = inputs_verdict(ctx, &active.acceptance[index], index)? {
                    return Err(message);
                }
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
                // the re-run is the second opinion (§2.1.2): same attested
                // state answering differently means flaky, not failed
                let paths = plan::digest_paths(&active);
                let state_before = plan::state_digest(&root, &paths, command);
                let run = exec::bash(ctx, command, Some(ACCEPTANCE_TIMEOUT_SECS), false);
                let state_after = plan::state_digest(&root, &paths, command);
                if !run.ok {
                    let command_text = command.to_string();
                    let flaky = state_before == state_after
                        && same_state_disagreement(
                            &active.acceptance[index],
                            &command_text,
                            &state_after,
                        );
                    if flaky {
                        if plan::apply_flaky(&mut active, index) {
                            let args = serde_json::json!({
                                "index": index,
                                "command": command_text,
                                "state_digest": state_after,
                            });
                            plan::commit(
                                &root,
                                &ctx.session_id,
                                &mut active,
                                "flaky",
                                "host",
                                true,
                                args,
                            )
                            .map_err(|e| format!("plan write failed: {e:#}"))?;
                        }
                        return Err(format!(
                            "flaky_check: acceptance {index} passed then failed on the same state: {command_text} — fix the flake or have the user waive it"
                        ));
                    }
                    return Err(format!(
                        "acceptance_failed: acceptance {index} no longer passes: {command_text} — {}",
                        run.output.lines().take(6).collect::<Vec<_>>().join(" / ")
                    ));
                }
            }
            plan::AcceptanceKind::Snapshot(command) => {
                // rung 4 re-runs like a command, but settles on frozen
                // output rather than on pass/fail
                if !plan::snapshot_current(&active.acceptance[index]) {
                    return Err(format!(
                        "no_snapshot: acceptance {index} has no frozen output to compare to: {command}"
                    ));
                }
                if let Some(message) = inputs_verdict(ctx, &active.acceptance[index], index)? {
                    return Err(message);
                }
                let frozen_hash = active.acceptance[index]
                    .snapshot
                    .as_ref()
                    .map(|frozen| frozen.output_hash.clone());
                let frozen_exit = active.acceptance[index]
                    .snapshot
                    .as_ref()
                    .and_then(|frozen| frozen.exit);
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
                let command_text = command.to_string();
                let paths = plan::digest_paths(&active);
                let state_before = plan::state_digest(&root, &paths, command);
                let run = exec::bash(ctx, command, Some(ACCEPTANCE_TIMEOUT_SECS), false);
                let state_after = plan::state_digest(&root, &paths, command);
                let output_hash = blake3::hash(run.output.as_bytes()).to_hex().to_string();
                if Some(output_hash.as_str()) == frozen_hash.as_deref()
                    && run.exit_code == frozen_exit
                {
                    continue;
                }
                let flaky = state_before == state_after
                    && same_state_disagreement(
                        &active.acceptance[index],
                        &command_text,
                        &state_after,
                    );
                if flaky {
                    if plan::apply_flaky(&mut active, index) {
                        let args = serde_json::json!({
                            "index": index,
                            "command": command_text,
                            "state_digest": state_after,
                        });
                        plan::commit(
                            &root,
                            &ctx.session_id,
                            &mut active,
                            "flaky",
                            "host",
                            true,
                            args,
                        )
                        .map_err(|e| format!("plan write failed: {e:#}"))?;
                    }
                    return Err(format!(
                        "flaky_check: acceptance {index} matched then differed on the same state: {command_text} — fix the flake or have the user waive it"
                    ));
                }
                return Err(format!(
                    "snapshot_changed: acceptance {index} output no longer matches: {command_text} — {}",
                    run.output.lines().take(6).collect::<Vec<_>>().join(" / ")
                ));
            }
            plan::AcceptanceKind::Differential(command) => {
                // rung 3 re-runs like a snapshot, but passes on changed
                // output and blocks on identical output
                if !plan::differential_current(&active.acceptance[index]) {
                    return Err(format!(
                        "no_snapshot: acceptance {index} has no frozen output to compare to: {command}"
                    ));
                }
                if let Some(message) = inputs_verdict(ctx, &active.acceptance[index], index)? {
                    return Err(message);
                }
                let frozen_hash = active.acceptance[index]
                    .snapshot
                    .as_ref()
                    .map(|frozen| frozen.output_hash.clone());
                let frozen_exit = active.acceptance[index]
                    .snapshot
                    .as_ref()
                    .and_then(|frozen| frozen.exit);
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
                let command_text = command.to_string();
                let run = exec::bash(ctx, command, Some(ACCEPTANCE_TIMEOUT_SECS), false);
                let output_hash = blake3::hash(run.output.as_bytes()).to_hex().to_string();
                if frozen_hash.as_deref() == Some(output_hash.as_str())
                    && frozen_exit == run.exit_code
                {
                    return Err(format!(
                        "no_observable_change: acceptance {index} output identical to the pre-change run: {command_text}"
                    ));
                }
                // changed output settles only a working change here too
                if run.exit_code != Some(0) {
                    return Err(format!(
                        "broken_change: acceptance {index} output changed but exits {:?}: {command_text}",
                        run.exit_code
                    ));
                }
            }
            plan::AcceptanceKind::Signatures(paths) => {
                // rung 5 re-reads like verify does, without executing
                // anything: identical shapes pass, anything else blocks
                let canonical = paths.join(", ");
                if !plan::signatures_current(&active.acceptance[index]) {
                    return Err(format!(
                        "no_signatures: acceptance {index} has no frozen shapes to compare to: {canonical}"
                    ));
                }
                let frozen = active.acceptance[index]
                    .shape
                    .as_ref()
                    .expect("gated above");
                let current = read_shapes(ctx, &frozen.files);
                let changed: Vec<&str> = frozen
                    .files
                    .iter()
                    .zip(current.iter())
                    .filter(|(frozen_file, (_, hash))| {
                        hash.as_deref() != Some(frozen_file.shape_hash.as_str())
                    })
                    .map(|(frozen_file, _)| frozen_file.path.as_str())
                    .collect();
                if !changed.is_empty() {
                    return Err(format!(
                        "signatures_changed: acceptance {index} declaration shapes changed: {}",
                        changed.join(", ")
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
                    &active.acceptance[index].evidence,
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
        // Kindless: what the step was *for* is the model's business. What
        // counts is that the records attest successful work — a failed exec
        // or errored diagnostics proves nothing (#198, §2.1.4). Diagnostics
        // count only with zero errors: the endpoint does not enforce that.
        let allowed = record.kind == "file_diff"
            || (record.kind == "tool_result"
                && record.fields.get("ok").and_then(Value::as_bool) == Some(true))
            || (record.kind == "diagnostics"
                && record.fields.get("errors").and_then(Value::as_u64) == Some(0));
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
            "wrong_evidence: evidence attests no successful work for step {step_id} — attach a successful exec, a file_diff, or zero-error diagnostics"
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

    /// Whole-plan abandon is the user's call (`/plan abandon`): the tool
    /// refuses it for the model, so tests retire plans this way directly.
    fn abandon_as_user(ctx: &ToolCtx, dir: &std::path::Path) {
        let mut active = plan::open_active(dir).unwrap().unwrap();
        let id = active.id.clone();
        plan::abandon(&mut active);
        plan::commit(
            dir,
            &ctx.session_id,
            &mut active,
            "cancel",
            "user",
            true,
            serde_json::json!({"id": id}),
        )
        .unwrap();
    }

    fn plan_with(acceptance: Vec<&str>) -> plan::Plan {        plan::create(
            "prove the checks".to_string(),
            Vec::new(),
            acceptance.into_iter().map(str::to_string).collect(),
            vec![plan::NewStep {
                title: "do the work".to_string(),
                refs: Vec::new(),
            }],
            0,
            &plan::Limits::default(),
        )
        .unwrap()
    }

    #[test]
    fn baselines_keep_the_failing_runs_and_only_those() {
        let (mut ctx, dir) = proj();
        let plan = plan_with(vec![
            "cmd: exit 3",
            "cmd: exit 0",
            "manual: eyeball it",
            "snapshot: exit 0",
        ]);
        let proof = capture_baselines(&mut ctx, &plan);
        assert_eq!(proof.slots.len(), 4);
        let baseline = proof.slots[0]
            .as_ref()
            .expect("a check that fails is the whole point");
        assert_eq!(baseline.exit, 3);
        // the hash is over the stripped command, the text the host runs
        assert_eq!(
            baseline.check_definition_hash,
            plan::check_definition_hash("exit 3")
        );
        assert!(!baseline.output_hash.is_empty());
        assert!(proof.slots[1].is_none(), "a check that passes proves nothing");
        assert!(proof.slots[2].is_none(), "manual items are never run");
        assert!(
            proof.frozen[3].is_none(),
            "empty output freezes nothing: {:?}",
            proof.notes
        );
        // and the model is told, per item, so it can fix the plan now
        assert!(proof.notes.iter().any(|n| n.contains("fails before")));
        assert!(
            proof.notes.iter().any(|n| n.contains("exit 3")),
            "the note carries the reason it failed: {:?}",
            proof.notes
        );
        assert!(
            proof.notes.iter().any(|n| n.contains("passes already")
                || n.contains("could not be run")
                || n.contains("empty output")),
            "the passing check must be named: {:?}",
            proof.notes
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_check_that_never_started_is_not_a_baseline_where_the_shell_says_so() {
        // Only POSIX shells report "not found" in the exit code. cmd.exe and
        // PowerShell use 1, the same code an ordinary failure uses, so there a
        // typo *is* recorded — and can never settle anything, because `verify`
        // needs the check to pass. The portable guard is the failing output
        // that rides the baseline and the create note.
        if !matches!(
            crate::agent::shell::ShellKind::detect(),
            crate::agent::shell::ShellKind::Bash | crate::agent::shell::ShellKind::Sh
        ) {
            return;
        }
        let (mut ctx, dir) = proj();
        let plan = plan_with(vec!["cmd: sqwai-no-such-command-xyz"]);
        let proof = capture_baselines(&mut ctx, &plan);
        assert!(
            proof.slots[0].is_none(),
            "a typo must not become evidence: {:?}",
            proof.notes
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn verify_refuses_a_check_that_has_never_failed() {
        let (mut ctx, dir) = proj();
        let mut plan = plan_with(vec!["cmd: exit 3"]);
        plan.sessions = vec![ctx.session_id.clone()];
        plan::store(&dir, &plan).unwrap();

        let denied = verify_acceptance(&mut ctx, 0, false);
        assert!(!denied.ok);
        assert!(
            denied.output.contains("no_baseline"),
            "the refusal names the rule: {}",
            denied.output
        );

        // with the proof attached the same call gets past the gate and runs
        // the check for real — which still fails, so nothing is settled
        plan.acceptance[0].baseline = Some(plan::Baseline {
            at: plan::now(),
            exit: 3,
            check_definition_hash: plan::check_definition_hash("exit 3"),
            output_hash: "hash".to_string(),
            head: "boom".to_string(),
            state_digest: "digest".to_string(),
        });
        plan::store(&dir, &plan).unwrap();
        let still_red = verify_acceptance(&mut ctx, 0, false);
        assert!(!still_red.ok);
        assert!(
            still_red.output.contains("acceptance_failed"),
            "a red check settles nothing: {}",
            still_red.output
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unsafe_acceptance_check_is_never_run_to_prove_itself() {
        let (mut ctx, dir) = proj();
        let plan = plan_with(vec!["cmd: rm -rf /"]);
        let proof = capture_baselines(&mut ctx, &plan);
        assert!(proof.slots[0].is_none());
        assert!(
            proof.notes.iter().any(|n| n.contains("not run")),
            "an unsafe check is refused, not executed: {:?}",
            proof.notes
        );
        fs::remove_dir_all(&dir).ok();
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
        assert!(outside.output.contains("subagent_scope"), "{}", outside.output);

        let read = execute(&mut ctx, "read", &json!({"file_path": "README.md"}));
        assert!(read.ok, "{}", read.output);
        assert!(!dir.join("notes.txt").exists());
        fs::remove_dir_all(&dir).ok();
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
        let merged = merge_specs(vec![spec("read"), spec("write")], &[spec("zzz"), spec("aaa")]);
        let names: Vec<String> = merged.iter().map(|t| t.name.clone()).collect();
        assert_eq!(names, vec!["aaa", "read", "write", "zzz"]);
        // server order must not leak through: reversed input, same output
        let merged = merge_specs(vec![spec("read"), spec("write")], &[spec("aaa"), spec("zzz")]);
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

    /// G0 baseline (§8.2): the durable machinery is invisible to the model.
    #[test]
    fn baseline_hides_durable_machinery_tools() {
        struct BaselineGuard;
        impl Drop for BaselineGuard {
            fn drop(&mut self) {
                crate::bench::set_baseline_override(None);
            }
        }
        let _guard = BaselineGuard;
        crate::bench::set_baseline_override(Some(true));
        let names: Vec<String> = tool_specs(false).iter().map(|t| t.name.clone()).collect();
        for hidden in [
            "plan",
            "propose_plan",
            "note",
            "journal",
            "memory_propose",
            "memory_read",
        ] {
            assert!(
                !names.contains(&hidden.to_string()),
                "{hidden} must be hidden on baseline: {names:?}"
            );
        }
        for kept in ["read", "bash", "resolve_ref", "recall"] {
            assert!(
                names.contains(&kept.to_string()),
                "{kept} must stay on baseline: {names:?}"
            );
        }
    }

    /// One schema set in every mode: the tool block is part of the request
    /// prefix, so a mode-dependent set re-keys the cache on every Plan/Act
    /// toggle. Plan mode refuses mutating calls at dispatch instead
    /// (`is_mutating_call`); the schemas stay identical so the prefix does.
    #[test]
    fn tool_schemas_are_mode_independent() {
        let names = |plan_mode: bool| {
            let mut names: Vec<String> =
                tool_specs(plan_mode).iter().map(|t| t.name.clone()).collect();
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

    /// Read-only bash classification: the observed inspection shapes pass,
    /// anything that could write fails closed. Advisory only — the plan
    /// gate consults it, approvals do not.
    #[test]
    fn readonly_bash_covers_inspection_but_nothing_else() {
        let bash = |command: &str| {
            is_readonly_bash("bash", &serde_json::json!({"command": command}))
        };
        // observed read-only shapes from a real inspection session
        assert!(bash(
            "powershell -NoProfile -Command \"Get-Process | Sort-Object CPU -Descending | Select-Object -First 25 Name, Id\""
        ));
        assert!(bash(
            "powershell -NoProfile -Command \"Get-Process | Where-Object { $_.Path } | Select-Object Name\""
        ));
        assert!(bash("netstat -ano | findstr LISTENING"));
        assert!(bash("netstat -ano | findstr ESTABLISHED"));
        assert!(bash("schtasks /query /FO TABLE | more"));
        assert!(bash("reg query HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run"));
        assert!(bash("tasklist /FI \"PID eq 20912\" /FO TABLE"));
        assert!(bash(
            "powershell -NoProfile -Command \"Get-MpComputerStatus | Select-Object AntivirusEnabled\""
        ));
        // writes, chains into writes, redirect, subexpressions: all mutating
        assert!(!bash("Get-Process; Remove-Item C:\\temp\\x"));
        assert!(!bash("echo hi > out.txt"));
        assert!(!bash("powershell -NoProfile -Command \"Get-Process\" | Out-File x.txt"));
        assert!(!bash("powershell -c \"rm foo\""));
        assert!(!bash("netstat -ano & del C:\\t"));
        assert!(!bash("powershell -Command \"$(rm foo)\""));
        assert!(!bash("schtasks /delete /TN x /F"));
        assert!(!bash("reg add HKCU\\x /v y"));
        assert!(!bash("date 01-01-25"));
        assert!(!bash(""));
        assert!(!bash("   "));
        // not bash at all
        assert!(!is_readonly_bash("read", &serde_json::json!({"file_path": "a"})));
        assert!(!is_readonly_bash("bash", &serde_json::json!({})));
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
            &json!({"op": "create", "goal": "diagnostics as evidence", "acceptance": [],
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

    /// Z write-path gate: a mutation outside the holding step's refs
    /// carries a scope warning; a mutation inside them stays clean.
    /// Refs are attached directly to dodge the create-time graph
    /// validator — the gate under test reads them, it does not validate.
    #[test]
    fn write_path_scope_gate_warns_outside_step_refs() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({"op": "create", "goal": "scoped work", "acceptance": [],
                    "steps": [{"title": "touch a"}]}),
        );
        assert!(created.ok, "{}", created.output);
        ctx.current_step = Some("1".into());
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        // refs after start: the start-time graph validator already passed,
        // and the gate under test only reads them.
        let mut active = plan::open_active(&dir).unwrap().unwrap();
        active.steps[0].refs = vec![crate::plan::StepRef::from("src/a.rs")];
        plan::store(&dir, &active).unwrap();

        let inside = execute(
            &mut ctx,
            "write",
            &json!({"file_path": "src/a.rs", "content": "hello\n"}),
        );
        assert!(inside.ok, "{}", inside.output);
        assert!(
            !inside.output.contains("outside this step's refs"),
            "{}",
            inside.output
        );

        let outside = execute(
            &mut ctx,
            "write",
            &json!({"file_path": "src/b.rs", "content": "hello\n"}),
        );
        assert!(outside.ok, "gate warns, never blocks: {}", outside.output);
        assert!(
            outside.output.contains("outside this step's refs"),
            "{}",
            outside.output
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// AF write-path gate: trap-shaped content (branch on a long literal)
    /// is flagged in the outcome; ordinary code passes silently.
    #[test]
    fn write_path_hardcode_gate_flags_trap_shaped_content() {
        let (mut ctx, dir) = proj();
        let trapped = execute(
            &mut ctx,
            "write",
            &json!({"file_path": "src/trap.rs",
                    "content": "fn check(input: &str) -> bool {\n    if input == \"the quick brown fixture bytes 0123456789\" {\n        return true;\n    }\n    false\n}\n"}),
        );
        assert!(trapped.ok, "{}", trapped.output);
        assert!(
            trapped.output.contains("test-shaped literal"),
            "{}",
            trapped.output
        );

        let plain = execute(
            &mut ctx,
            "write",
            &json!({"file_path": "src/plain.rs", "content": "fn f(x: i32) -> i32 {\n    x + 1\n}\n"}),
        );
        assert!(plain.ok, "{}", plain.output);
        assert!(
            !plain.output.contains("test-shaped literal"),
            "{}",
            plain.output
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// H1 executor sandbox (§12.7): writers, plan, notes and gated bash
    /// refuse with `reflector_read_only`; reads and safe commands proceed.
    #[test]
    fn reflector_ctx_is_verify_only() {
        let (mut ctx, dir) = proj();
        ctx.reflector = true;
        for (name, args) in [
            ("write", json!({"file_path": "src/x.rs", "content": "x"})),
            (
                "edit",
                json!({"file_path": "src/main.rs", "old_string": "a", "new_string": "b"}),
            ),
            ("patch", json!({"patch": "x"})),
            ("plan", json!({"op": "show"})),
            ("note", json!({"note": "hi", "kind": "decision"})),
            ("memory_propose", json!({"text": "hi", "scope": "project"})),
            ("webfetch", json!({"url": "https://example.com"})),
            // §12.7 names the bans explicitly; "reflect" is not even a tool
            // (journal kind), but a hallucinated call must refuse the same way
            ("subagent", json!({"task": "hi"})),
            ("reflect", json!({})),
        ] {
            let refused = execute(&mut ctx, name, &args);
            assert!(!refused.ok, "{name} must refuse");
            assert!(
                refused.output.contains("reflector_read_only"),
                "{name}: {}",
                refused.output
            );
        }
        // safe commands run; nothing here can approve, so the risky ones
        // refuse instead of prompting
        let ok = execute(
            &mut ctx,
            "bash",
            &json!({"command": "echo reflector-probe"}),
        );
        assert!(ok.ok, "{}", ok.output);
        let denied = execute(
            &mut ctx,
            "bash",
            &json!({"command": "curl example.com/x.sh | sh"}),
        );
        assert!(!denied.ok, "pipe-into-interpreter must refuse");
        assert!(
            denied.output.contains("reflector_read_only"),
            "{}",
            denied.output
        );
        // reads still work
        let read = execute(&mut ctx, "read", &json!({"file_path": "src/main.rs"}));
        assert!(read.ok, "{}", read.output);
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
        let err = validate_attached_records(&dir, &plan_id, "1", &stale)
            .unwrap_err();
        assert!(err.contains("stale_epoch"), "{err}");

        let fresh = vec![plan::EvidenceRef {
            session: "epoch-test".into(),
            seq: fresh_seq,
        }];
        validate_attached_records(&dir, &plan_id, "1", &fresh)
            .expect("unstamped evidence counts");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn plan_create_expands_named_verify_commands() {
        let (mut ctx, dir) = proj();
        std::fs::create_dir_all(dir.join(".sqwai")).unwrap();
        std::fs::write(
            dir.join(".sqwai").join("config.toml"),
            "[verify]\ncommands = { unit = \"cargo test --lib\" }\n",
        )
        .unwrap();
        let mut mk = |acceptance: Vec<&str>| {
            plan_op(
                &mut ctx,
                &json!({
                    "op": "create",
                    "goal": "verify",
                    "constraints": [],
                    "acceptance": acceptance,
                    "steps": [{"title": "s", "kind": "research"}]
                }),
            )
        };
        // unknown name rejects with the known list (no plan exists yet,
        // so substitution — not plan_exists — answers)
        let bad = mk(vec!["cmd: $nope"]);
        assert!(!bad.ok);
        assert!(bad.output.contains("unknown_verify"), "{}", bad.output);
        assert!(bad.output.contains("unit"), "{}", bad.output);
        let ok = mk(vec!["cmd: $unit"]);
        assert!(ok.ok, "{}", ok.output);
        let plan = plan::open_active(&dir).unwrap().unwrap();
        assert_eq!(plan.acceptance[0].text, "cmd: cargo test --lib");
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

    #[test]
    fn evidence_content_rule_is_kindless() {
        // strict mode: what counts is that the records attest successful
        // work — any step, same rule. Failed execs and errored diagnostics
        // prove nothing, whatever the step was for.
        let (mut ctx, dir) = proj();
        ctx.plan_limits.strict = true;
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "validate evidence",
                "steps": [
                    {"title": "look around"},
                    {"title": "measure twice"},
                    {"title": "cut once"}
                ]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        let mut journal = crate::agent::journal::Journal::open(&dir, &ctx.session_id).unwrap();

        // failed-only exec evidence settles nothing
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        journal.set_attribution(Some("1".into()), Some(plan_id.clone()), "main");
        journal.append("plan", json!({"op": "start"})).unwrap();
        journal
            .append_evidence("tool_result", json!({"tool": "read", "ok": false}))
            .unwrap();
        let rejected = plan_op(
            &mut ctx,
            &json!({"op": "finish", "id": "1", "summary": "looked"}),
        );
        assert!(!rejected.ok);
        assert!(
            rejected.output.contains("wrong_evidence"),
            "{}",
            rejected.output
        );
        // a refused finish leaves the step open; cancel it to move on
        assert!(
            plan_op(&mut ctx, &json!({"op": "cancel", "id": "1", "reason": "no evidence"}))
                .ok
        );

        // errored diagnostics settle nothing either
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "2"})).ok);
        journal.set_attribution(Some("2".into()), Some(plan_id.clone()), "main");
        journal.append("plan", json!({"op": "start"})).unwrap();
        journal
            .append_evidence(
                "diagnostics",
                json!({"path": "src/main.rs", "errors": 2, "warnings": 0}),
            )
            .unwrap();
        let rejected = plan_op(
            &mut ctx,
            &json!({"op": "finish", "id": "2", "summary": "measured"}),
        );
        assert!(!rejected.ok);
        assert!(
            rejected.output.contains("wrong_evidence"),
            "{}",
            rejected.output
        );
        assert!(
            plan_op(&mut ctx, &json!({"op": "cancel", "id": "2", "reason": "no evidence"}))
                .ok
        );

        // a recorded write settles any step
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "3"})).ok);
        journal.set_attribution(Some("3".into()), Some(plan_id.clone()), "main");
        journal.append("plan", json!({"op": "start"})).unwrap();
        journal
            .append_evidence("file_diff", json!({"path": "src/main.rs"}))
            .unwrap();
        let done = plan_op(
            &mut ctx,
            &json!({"op": "finish", "id": "3", "summary": "cut"}),
        );
        assert!(done.ok, "{}", done.output);
        fs::remove_dir_all(&dir).ok();
    }

    /// Strict mode keeps the #198 bar — failed calls are not evidence —
    /// while soft steps close on a summary alone. Same journal, two gates.
    #[test]
    fn strict_finish_demands_successful_evidence() {
        let (mut ctx, dir) = proj();
        ctx.plan_limits.strict = true;
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "validate research",
                "steps": [{"title": "look around"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        let mut journal = crate::agent::journal::Journal::open(&dir, &ctx.session_id).unwrap();

        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        journal.set_attribution(Some("1".into()), Some(plan_id.clone()), "main");
        journal.append("plan", json!({"op": "start"})).unwrap();
        journal
            .append_evidence("tool_result", json!({"tool": "read", "ok": false}))
            .unwrap();
        let rejected = plan_op(
            &mut ctx,
            &json!({"op": "finish", "id": "1", "summary": "looked"}),
        );
        assert!(
            !rejected.ok,
            "failed calls are not evidence: {}",
            rejected.output
        );
        assert!(
            rejected.output.contains("wrong_evidence"),
            "{}",
            rejected.output
        );

        // one successful call flips it to acceptable
        journal
            .append_evidence("tool_result", json!({"tool": "read", "ok": true}))
            .unwrap();
        let done = plan_op(
            &mut ctx,
            &json!({"op": "finish", "id": "1", "summary": "found"}),
        );
        assert!(done.ok, "{}", done.output);
        fs::remove_dir_all(&dir).ok();
    }

    /// Soft steps (the default) close on a summary alone — no journal
    /// evidence needed. Progress is read from receipts, not from gates.
    #[test]
    fn soft_finish_closes_on_summary_alone() {
        let (mut ctx, dir) = proj();
        assert!(!ctx.plan_limits.strict, "soft is the default");
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
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

    /// A `cmd:` acceptance item is settled by the host running the command,
    /// not by pointing at a journal record. This test used to pass a fabricated
    /// `bash` result as evidence for `cmd: cargo test` and see the item
    /// verified — the suite never ran.
    ///
    /// §12.12: a check only settles what it could fail. Both probes miss
    /// before the change (so both take a baseline); one still misses at
    /// verify and one is fixed first, so the same test covers the red
    /// and the green path through the real runner.
    #[test]
    fn cmd_acceptance_is_verified_by_running_the_command() {
        let (mut ctx, dir) = proj();
        let flag_fail = dir.join("fail-gate.txt");
        let flag_pass = dir.join("pass-gate.txt");
        let cmd_fail = gate_probe_command(&flag_fail);
        let cmd_pass = gate_probe_command(&flag_pass);
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "verify acceptance",
                "acceptance": [format!("cmd: {cmd_fail}"), format!("cmd: {cmd_pass}")],
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

        // fix the second check after the baseline was taken: now it passes
        fs::write(&flag_pass, "ok").unwrap();
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
    ///
    /// §12.12: an unsafe command never gets a baseline (it is not run to
    /// prove itself), so `verify` refuses it as unproven first. With a
    /// forged-in proof the same item must still be refused as unsafe —
    /// the safety gate stays behind the baseline gate.
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
        assert!(out.output.contains("no_baseline"), "{}", out.output);

        // even with proof smuggled in, the command must not run
        let mut plan = plan::open_active(&dir).unwrap().unwrap();
        plan.acceptance[0].baseline = Some(plan::Baseline {
            at: plan::now(),
            exit: 1,
            check_definition_hash: plan::check_definition_hash("rm -rf /"),
            output_hash: "hash".to_string(),
            head: "boom".to_string(),
            state_digest: "digest".to_string(),
        });
        plan::store(&dir, &plan).unwrap();
        let unsafe_out = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!unsafe_out.ok, "{}", unsafe_out.output);
        assert!(
            unsafe_out.output.contains("unsafe_acceptance"),
            "{}",
            unsafe_out.output
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

    /// Untyped acceptance cannot be created: free text settles on whatever
    /// evidence happens to exist, which is a claim, not a check. The
    /// rejection names the typed alternatives.
    #[test]
    fn create_refuses_untyped_acceptance() {
        let (mut ctx, dir) = proj();
        let rejected = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "vague criteria",
                "acceptance": ["the suite is green"],
                "steps": [{"title": "verify"}]
            }),
        );
        assert!(!rejected.ok, "{}", rejected.output);
        assert!(
            rejected.output.contains("untyped_acceptance"),
            "{}",
            rejected.output
        );
        assert!(rejected.output.contains("cmd:"), "{}", rejected.output);
        // and nothing was stored
        assert!(plan::open_active(&dir).unwrap().is_none());
        fs::remove_dir_all(&dir).ok();
    }

    /// Ladder walk surfaces at create time: a manual-only plan engages no
    /// executable rung, and the create result says so. No shell runs here —
    /// manual items are never executed — so this also pins the walk as
    /// shell-free.
    #[test]
    fn create_names_the_ladder_rung_or_its_absence() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "eyeball it",
                "acceptance": ["manual: the panel looks right"],
                "steps": [{"title": "verify", "kind": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        assert!(created.output.contains("no executable rung"), "{}", created.output);

        let plan = plan::open_active(&dir).unwrap().unwrap();
        assert_eq!(plan::ladder_top(&plan), None);
        assert!(!plan::render(&plan).contains("[rung"));
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

    /// Dump a file's bytes to stdout, spelled per platform like
    /// [`gate_probe_command`]: `cat` where a POSIX shell runs the suite,
    /// `Get-Content` where PowerShell does.
    #[cfg(unix)]
    fn dump_command(path: &std::path::Path) -> String {
        format!("cat {}", path.display())
    }

    /// Windows spelling of [`dump_command`], same quoting discipline as
    /// [`gate_probe_command`].
    #[cfg(windows)]
    fn dump_command(path: &std::path::Path) -> String {
        format!(
            "powershell -NoProfile -Command Get-Content '{}'",
            path.display()
        )
    }

    /// A command whose output differs on every run, spelled per platform:
    /// the shell's own PID where a POSIX shell runs the suite, the clock
    /// where Cmd does. Used to prove a freeze refuses nondeterministic
    /// inputs rather than blessing them.
    #[cfg(unix)]
    fn clock_command() -> String {
        "echo $$".to_string()
    }

    /// Cmd spelling of [`clock_command`]: `%TIME%` ticks in centiseconds,
    /// and two process spawns never land in the same one. (No PowerShell
    /// here: its scriptlets trip the safety classifier — `-Format` even
    /// matches the destructive-disk heuristic.)
    #[cfg(windows)]
    fn clock_command() -> String {
        "echo %TIME%".to_string()
    }

    /// `complete` runs `cmd:` items again instead of trusting the verify that
    /// happened earlier: a criterion that stopped passing must block
    /// completion (§2.1.2).
    ///
    /// §12.12 three states: the probe misses before the change (baseline),
    /// is fixed, verifies green, then the tracked world visibly moves and
    /// the check breaks with it — a regression on moved state, so
    /// `acceptance_failed`, not `flaky_check`. `gate.txt` is tracked
    /// throughout (the step refs validate); the probe watches a second
    /// file outside the digest, whose removal is the breakage.
    #[test]
    fn complete_reruns_cmd_acceptance_and_refuses_when_it_now_fails() {
        let (mut ctx, dir) = proj();
        fs::write(dir.join("gate.txt"), "ok").unwrap();
        let probe = dir.join("probe.txt");
        let command = gate_probe_command(&probe);
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "completion re-checks",
                "acceptance": [format!("cmd: {command}")],
                "steps": [{"title": "verify", "kind": "verify", "refs": ["gate.txt"]}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        // the fix happens after the baseline was taken
        fs::write(&probe, "ok").unwrap();
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);

        let verified = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(verified.ok, "{}", verified.output);

        // the world visibly moved (tracked digest) and the check broke with
        // it — a regression, so the item stays verified-but-broken, not flaky
        fs::write(dir.join("gate.txt"), "tampered").unwrap();
        fs::remove_file(&probe).unwrap();
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
        assert_eq!(
            plan::open_active(&dir)
                .unwrap()
                .unwrap()
                .acceptance[0]
                .validation
                .status,
            plan::ValidationStatus::Passed,
            "a regression on moved state is not a flake"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// §12.12 three states, verify side: a green run attested the state,
    /// and now the same state answers red with nothing tracked moving —
    /// the runs disagree, so the item is flaky rather than failed. A
    /// further green run must not silently retry it into verified, and
    /// `complete` stays blocked.
    #[test]
    fn verify_disagreeing_rerun_on_same_state_is_flaky() {
        let (mut ctx, dir) = proj();
        let flag = dir.join("flag.txt");
        let command = gate_probe_command(&flag);
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "flaky check",
                "acceptance": [format!("cmd: {command}")],
                "steps": [{"title": "verify", "kind": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        // the fix happens after the baseline was taken
        fs::write(&flag, "ok").unwrap();
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        let verified = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(verified.ok, "{}", verified.output);

        // the flag is gone but no tracked state moved (no refs): the same
        // attested state now answers differently
        fs::remove_file(&flag).unwrap();
        let flaky = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!flaky.ok, "{}", flaky.output);
        assert!(flaky.output.contains("flaky_check"), "{}", flaky.output);
        let plan = plan::open_active(&dir).unwrap().unwrap();
        assert_eq!(plan.acceptance[0].status, plan::AcceptanceStatus::Passed);
        assert_eq!(
            plan.acceptance[0].validation.status,
            plan::ValidationStatus::Unknown
        );
        assert_eq!(
            plan.acceptance[0].validation.receipts.len(),
            1,
            "a red run issues no receipt"
        );
        assert!(
            plan::render(&plan).contains("flaky"),
            "{}",
            plan::render(&plan)
        );

        // never silently retried into verified: no run is spent, same refusal
        let again = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!again.ok, "{}", again.output);
        assert!(again.output.contains("flaky_check"), "{}", again.output);

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
            completed.output.contains("flaky_check"),
            "{}",
            completed.output
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// §12.12 three states, verify side, the other half of the
    /// distinguisher: the red run lands on visibly moved state, so it is
    /// a regression (`acceptance_failed`) and the item is not marked flaky.
    #[test]
    fn verify_red_on_moved_state_is_regression_not_flake() {
        let (mut ctx, dir) = proj();
        fs::write(dir.join("gate.txt"), "ok").unwrap();
        let probe = dir.join("probe.txt");
        let command = gate_probe_command(&probe);
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "regression check",
                "acceptance": [format!("cmd: {command}")],
                "steps": [{"title": "verify", "kind": "verify", "refs": ["gate.txt"]}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        fs::write(&probe, "ok").unwrap();
        let verified = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(verified.ok, "{}", verified.output);

        fs::write(dir.join("gate.txt"), "tampered").unwrap();
        fs::remove_file(&probe).unwrap();
        let failed = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!failed.ok, "{}", failed.output);
        assert!(
            failed.output.contains("acceptance_failed"),
            "{}",
            failed.output
        );
        assert_eq!(
            plan::open_active(&dir)
                .unwrap()
                .unwrap()
                .acceptance[0]
                .validation
                .status,
            plan::ValidationStatus::Passed,
            "a regression on moved state is not a flake"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// §12.12 three states, complete side: the re-run at `complete` is the
    /// run that disagrees — same attested state, red answer — so the item
    /// is marked flaky there instead of failing as a regression.
    #[test]
    fn complete_disagreeing_rerun_on_same_state_is_flaky() {
        let (mut ctx, dir) = proj();
        let flag = dir.join("flag.txt");
        let command = gate_probe_command(&flag);
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "flaky at completion",
                "acceptance": [format!("cmd: {command}")],
                "steps": [{"title": "verify", "kind": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        fs::write(&flag, "ok").unwrap();
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        let verified = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(verified.ok, "{}", verified.output);

        // the flag is gone but no tracked state moved: the complete re-run
        // disagrees with the verify run on the same state
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
            completed.output.contains("flaky_check"),
            "{}",
            completed.output
        );
        assert_eq!(
            plan::open_active(&dir)
                .unwrap()
                .unwrap()
                .acceptance[0]
                .validation
                .status,
            plan::ValidationStatus::Unknown
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// Rung 4, the green path: the host froze `data.txt` at plan time, the
    /// content is unchanged, so the re-run matches byte for byte and the
    /// item verifies with an exec receipt carrying the frozen output hash.
    #[test]
    fn snapshot_freezes_output_and_verifies_on_match() {
        let (mut ctx, dir) = proj();
        fs::write(dir.join("data.txt"), "v1").unwrap();
        let command = dump_command(&dir.join("data.txt"));
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "freeze the behavior",
                "acceptance": [format!("snapshot: {command}")],
                "steps": [{"title": "verify", "kind": "verify", "refs": ["data.txt"]}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        assert!(created.output.contains("frozen"), "{}", created.output);

        let verified = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(verified.ok, "{}", verified.output);
        let plan = plan::open_active(&dir).unwrap().unwrap();
        assert_eq!(plan.acceptance[0].status, plan::AcceptanceStatus::Passed);
        assert_eq!(plan.acceptance[0].validation.receipts.len(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    /// Rung 4 refuses vacuous freezes: `exit 0` prints nothing, and empty
    /// output discriminates nothing — the item stays unfrozen and `verify`
    /// says `no_snapshot`, the snapshot analogue of `no_baseline`.
    #[test]
    fn snapshot_empty_output_freezes_nothing() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "vacuous freeze",
                "acceptance": ["snapshot: exit 0"],
                "steps": [{"title": "verify", "kind": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        assert!(created.output.contains("empty output"), "{}", created.output);

        let out = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!out.ok, "{}", out.output);
        assert!(out.output.contains("no_snapshot"), "{}", out.output);
        fs::remove_dir_all(&dir).ok();
    }

    /// Rung 4, three states: the frozen run attested one digest and the
    /// content changed under it with no tracked state moving — the runs
    /// disagree, so the item is flaky rather than changed.
    #[test]
    fn snapshot_changed_output_on_same_state_is_flaky() {
        let (mut ctx, dir) = proj();
        fs::write(dir.join("data.txt"), "v1").unwrap();
        let command = dump_command(&dir.join("data.txt"));
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "nondeterministic output",
                "acceptance": [format!("snapshot: {command}")],
                "steps": [{"title": "verify", "kind": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let verified = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(verified.ok, "{}", verified.output);

        // data.txt is untracked here, so the digest cannot see the change:
        // same attested state, different output
        fs::write(dir.join("data.txt"), "v2").unwrap();
        let flaky = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!flaky.ok, "{}", flaky.output);
        assert!(flaky.output.contains("flaky_check"), "{}", flaky.output);
        assert_eq!(
            plan::open_active(&dir)
                .unwrap()
                .unwrap()
                .acceptance[0]
                .validation
                .status,
            plan::ValidationStatus::Unknown
        );

        // never silently retried into verified
        let again = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!again.ok, "{}", again.output);
        assert!(again.output.contains("flaky_check"), "{}", again.output);
        fs::remove_dir_all(&dir).ok();
    }

    /// Rung 4, the regression half: `data.txt` is tracked, so the content
    /// change moves the digest and the differing output is changed behavior
    /// (`snapshot_changed`), not a flake.
    #[test]
    fn snapshot_changed_output_on_moved_state_fails() {
        let (mut ctx, dir) = proj();
        fs::write(dir.join("data.txt"), "v1").unwrap();
        let command = dump_command(&dir.join("data.txt"));
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "changed behavior",
                "acceptance": [format!("snapshot: {command}")],
                "steps": [{"title": "verify", "kind": "verify", "refs": ["data.txt"]}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let verified = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(verified.ok, "{}", verified.output);

        fs::write(dir.join("data.txt"), "v2").unwrap();
        let failed = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!failed.ok, "{}", failed.output);
        assert!(
            failed.output.contains("snapshot_changed"),
            "{}",
            failed.output
        );
        assert_eq!(
            plan::open_active(&dir)
                .unwrap()
                .unwrap()
                .acceptance[0]
                .validation
                .status,
            plan::ValidationStatus::Passed,
            "changed behavior on moved state is not a flake"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// Rung 4 through `complete`: unchanged output re-runs green and the
    /// plan completes; changed output on moved state blocks it.
    #[test]
    fn complete_reruns_snapshot_and_blocks_on_change() {
        let (mut ctx, dir) = proj();
        fs::write(dir.join("data.txt"), "v1").unwrap();
        let command = dump_command(&dir.join("data.txt"));
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "snapshot at completion",
                "acceptance": [format!("snapshot: {command}")],
                "steps": [{"title": "verify", "kind": "verify", "refs": ["data.txt"]}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        assert!(plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0})).ok);
        assert!(
            plan_op(
                &mut ctx,
                &json!({"op": "cancel", "id": "1", "reason": "done here"})
            )
            .ok
        );
        let completed = plan_op(&mut ctx, &json!({"op": "complete"}));
        assert!(completed.ok, "{}", completed.output);

        fs::remove_dir_all(&dir).ok();
    }

    /// An unsafe `snapshot:` check is never run to freeze itself: the item
    /// stays unfrozen (`no_snapshot`), and even a smuggled-in frozen output
    /// still meets the safety gate.
    #[test]
    fn snapshot_refuses_a_command_that_would_need_approval() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "sneak a command in",
                "acceptance": ["snapshot: rm -rf /"],
                "steps": [{"title": "verify", "kind": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);

        let out = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!out.ok, "{}", out.output);
        assert!(out.output.contains("no_snapshot"), "{}", out.output);

        let mut plan = plan::open_active(&dir).unwrap().unwrap();
        plan.acceptance[0].snapshot = Some(plan::Snapshot {
            at: plan::now(),
            exit: Some(0),
            check_definition_hash: plan::check_definition_hash("rm -rf /"),
            output_hash: "hash".to_string(),
            head: "boom".to_string(),
            state_digest: "digest".to_string(),
        });
        plan::store(&dir, &plan).unwrap();
        let unsafe_out = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!unsafe_out.ok, "{}", unsafe_out.output);
        assert!(
            unsafe_out.output.contains("unsafe_acceptance"),
            "{}",
            unsafe_out.output
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// Rung 3, the green path inverted from rung 4: the host froze `data.txt`
    /// at "v1"; unchanged content settles nothing (`no_observable_change`),
    /// and only moved content verifies.
    #[test]
    fn differential_freezes_stable_and_passes_on_change() {
        let (mut ctx, dir) = proj();
        fs::write(dir.join("data.txt"), "v1").unwrap();
        let command = dump_command(&dir.join("data.txt"));
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "move the behavior",
                "acceptance": [format!("differential: {command}")],
                "steps": [{"title": "verify", "kind": "verify", "refs": ["data.txt"]}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        assert!(created.output.contains("stable across 2 runs"), "{}", created.output);

        // unchanged output is not acceptance here
        let same = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!same.ok, "{}", same.output);
        assert!(
            same.output.contains("no_observable_change"),
            "{}",
            same.output
        );

        // moved output settles the item with an exec receipt
        fs::write(dir.join("data.txt"), "v2").unwrap();
        let verified = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(verified.ok, "{}", verified.output);
        let plan = plan::open_active(&dir).unwrap().unwrap();
        assert_eq!(plan.acceptance[0].status, plan::AcceptanceStatus::Passed);
        assert_eq!(plan.acceptance[0].validation.receipts.len(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    /// Rung 3 settles working changes: changed output with a non-zero exit
    /// is breakage (`broken_change`), not movement. Fixing an error
    /// (non-zero frozen, zero now) still passes.
    #[test]
    fn differential_broken_output_does_not_verify() {
        let (mut ctx, dir) = proj();
        let missing = dir.join("gone.txt");
        let present = dir.join("here.txt");
        fs::write(&present, "v1").unwrap();
        let fix_command = dump_command(&missing);
        let break_command = dump_command(&present);
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "working changes only",
                "acceptance": [format!("differential: {fix_command}"), format!("differential: {break_command}")],
                "steps": [{"title": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);

        // fixing an error: frozen missing (exit non-zero), now present
        fs::write(&missing, "v1").unwrap();
        let fixed = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(fixed.ok, "{}", fixed.output);

        // breaking a working check: frozen present (exit zero), now missing
        fs::remove_file(&present).unwrap();
        let broken = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 1}));
        assert!(!broken.ok, "{}", broken.output);
        assert!(
            broken.output.contains("broken_change"),
            "{}",
            broken.output
        );
        assert_eq!(
            plan::open_active(&dir).unwrap().unwrap().acceptance[1].status,
            plan::AcceptanceStatus::Pending
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// Rung 3 refuses nondeterministic inputs at freeze time: a clock reads
    /// differently on every run, so comparing against it would pass
    /// trivially. The double run catches that before anything is frozen.
    #[test]
    fn differential_nondeterministic_input_freezes_nothing() {
        let (mut ctx, dir) = proj();
        let command = clock_command();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "unstable input",
                "acceptance": [format!("differential: {command}")],
                "steps": [{"title": "verify", "kind": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        assert!(
            created.output.contains("differs run to run"),
            "{}",
            created.output
        );

        let out = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!out.ok, "{}", out.output);
        assert!(out.output.contains("no_snapshot"), "{}", out.output);
        fs::remove_dir_all(&dir).ok();
    }

    /// Rung 3 shares rung 4's vacuity rule: silent output discriminates
    /// nothing in either direction.
    #[test]
    fn differential_empty_output_freezes_nothing() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "vacuous differential",
                "acceptance": ["differential: exit 0"],
                "steps": [{"title": "verify", "kind": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        assert!(created.output.contains("empty output"), "{}", created.output);

        let out = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!out.ok, "{}", out.output);
        assert!(out.output.contains("no_snapshot"), "{}", out.output);
        fs::remove_dir_all(&dir).ok();
    }

    /// Rung 3 through `complete`: the re-run sees moved output and the plan
    /// completes; identical output blocks it.
    #[test]
    fn complete_reruns_differential_and_blocks_when_unchanged() {        let (mut ctx, dir) = proj();
        fs::write(dir.join("data.txt"), "v1").unwrap();
        let command = dump_command(&dir.join("data.txt"));
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "differential at completion",
                "acceptance": [format!("differential: {command}")],
                "steps": [{"title": "verify", "kind": "verify", "refs": ["data.txt"]}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);

        // unchanged: verify refuses, and so would complete
        let same = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!same.ok, "{}", same.output);
        assert!(
            same.output.contains("no_observable_change"),
            "{}",
            same.output
        );

        fs::write(dir.join("data.txt"), "v2").unwrap();
        assert!(plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0})).ok);
        assert!(
            plan_op(
                &mut ctx,
                &json!({"op": "cancel", "id": "1", "reason": "done here"})
            )
            .ok
        );
        let completed = plan_op(&mut ctx, &json!({"op": "complete"}));
        assert!(completed.ok, "{}", completed.output);
        fs::remove_dir_all(&dir).ok();
    }

    /// An unsafe `differential:` check is never run to freeze itself, and a
    /// smuggled-in frozen output still meets the safety gate.
    #[test]
    fn differential_refuses_a_command_that_would_need_approval() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "sneak a command in",
                "acceptance": ["differential: rm -rf /"],
                "steps": [{"title": "verify", "kind": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);

        let out = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!out.ok, "{}", out.output);
        assert!(out.output.contains("no_snapshot"), "{}", out.output);

        let mut plan = plan::open_active(&dir).unwrap().unwrap();
        plan.acceptance[0].snapshot = Some(plan::Snapshot {
            at: plan::now(),
            exit: Some(0),
            check_definition_hash: plan::check_definition_hash("rm -rf /"),
            output_hash: "hash".to_string(),
            head: "boom".to_string(),
            state_digest: "digest".to_string(),
        });
        plan::store(&dir, &plan).unwrap();
        let unsafe_out = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!unsafe_out.ok, "{}", unsafe_out.output);
        assert!(
            unsafe_out.output.contains("unsafe_acceptance"),
            "{}",
            unsafe_out.output
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// Rung 5, the green path: the host froze `src/main.rs` as one `main`.
    /// Editing the body keeps the shape and verifies; adding a function
    /// breaks it.
    #[test]
    fn signatures_freeze_and_verify_shape() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "hold the shape",
                "acceptance": ["signatures: src/main.rs"],
                "steps": [{"title": "verify", "kind": "verify", "refs": ["src/main.rs"]}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        assert!(created.output.contains("froze 1 file(s)"), "{}", created.output);

        let verified = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(verified.ok, "{}", verified.output);

        // bodies move freely: the shape is declarations, not bytes
        assert!(execute(&mut ctx, "read", &json!({"file_path": "src/main.rs"})).ok);
        assert!(
            execute(
                &mut ctx,
                "edit",
                &json!({"file_path": "src/main.rs", "old_string": "TODO", "new_string": "DONE"}),
            )
            .ok
        );
        let still_green = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(still_green.ok, "{}", still_green.output);

        // a new declaration breaks the freeze
        assert!(
            execute(
                &mut ctx,
                "edit",
                &json!({"file_path": "src/main.rs", "old_string": "fn main() {}", "new_string": "fn main() {}\nfn helper() {}"}),
            )
            .ok
        );
        let failed = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!failed.ok, "{}", failed.output);
        assert!(
            failed.output.contains("signatures_changed"),
            "{}",
            failed.output
        );
        assert!(
            failed.output.contains("src/main.rs"),
            "{}",
            failed.output
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// Rung 5 freezes nothing without files: a missing path leaves the
    /// whole item unfrozen (all-or-nothing, so a typo cannot silently
    /// narrow the commitment), and `verify` says `no_signatures`.
    #[test]
    fn signatures_missing_file_freezes_nothing() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "typo in the set",
                "acceptance": ["signatures: src/main.rs, src/nope.rs"],
                "steps": [{"title": "verify", "kind": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        assert!(created.output.contains("src/nope.rs"), "{}", created.output);

        let out = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!out.ok, "{}", out.output);
        assert!(out.output.contains("no_signatures"), "{}", out.output);
        let plan = plan::open_active(&dir).unwrap().unwrap();
        assert!(plan.acceptance[0].shape.is_none());
        assert!(plan::render(&plan).contains("[no signatures]"));
        fs::remove_dir_all(&dir).ok();
    }

    /// Rung 5 never reads through the host boundary: a `.sqwai` path is
    /// refused at freeze time like in every other file tool.
    #[test]
    fn signatures_refuses_host_owned_state() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "peek the journal",
                "acceptance": ["signatures: .sqwai/plans/x.json"],
                "steps": [{"title": "verify", "kind": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        assert!(created.output.contains("host-owned"), "{}", created.output);

        let out = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!out.ok, "{}", out.output);
        assert!(out.output.contains("no_signatures"), "{}", out.output);
        fs::remove_dir_all(&dir).ok();
    }

    /// Rung 5 through `complete`: the re-read sees a reshaped file and
    /// blocks completion.
    #[test]
    fn complete_reruns_signatures_and_blocks_on_reshape() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "shape at completion",
                "acceptance": ["signatures: src/main.rs"],
                "steps": [{"title": "verify", "kind": "verify", "refs": ["src/main.rs"]}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        assert!(plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0})).ok);

        // reshape after the verify: the complete re-read must catch it
        assert!(execute(&mut ctx, "read", &json!({"file_path": "src/main.rs"})).ok);
        assert!(
            execute(
                &mut ctx,
                "edit",
                &json!({"file_path": "src/main.rs", "old_string": "fn main() {}", "new_string": "fn main() {}\nfn helper() {}"}),
            )
            .ok
        );
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
            completed.output.contains("signatures_changed"),
            "{}",
            completed.output
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// Rung 5 through `complete`, the green half: the held shape re-reads
    /// clean and the plan completes.
    #[test]
    fn complete_signatures_pass_on_same_shape() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "held shape at completion",
                "acceptance": ["signatures: src/main.rs"],
                "steps": [{"title": "verify", "kind": "verify", "refs": ["src/main.rs"]}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        assert!(plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0})).ok);
        assert!(
            plan_op(
                &mut ctx,
                &json!({"op": "cancel", "id": "1", "reason": "done here"})
            )
            .ok
        );
        let completed = plan_op(&mut ctx, &json!({"op": "complete"}));
        assert!(completed.ok, "{}", completed.output);
        fs::remove_dir_all(&dir).ok();
    }

    /// Check inputs freeze at create: a `tests/` file present lands in the
    /// item's inputs and shows in render; `src/` files do not (the known
    /// Rust-unit-test gap — globs cannot isolate them).
    #[test]
    fn create_freezes_check_inputs() {
        let (mut ctx, dir) = proj();
        fs::create_dir_all(dir.join("tests")).unwrap();
        fs::write(dir.join("tests/auth.rs"), "fn t() {}\n").unwrap();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "frozen inputs",
                "acceptance": ["cmd: exit 3"],
                "steps": [{"title": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let plan = plan::open_active(&dir).unwrap().unwrap();
        assert_eq!(
            plan.acceptance[0]
                .inputs
                .iter()
                .map(|i| i.path.clone())
                .collect::<Vec<_>>(),
            vec!["tests/auth.rs".to_string()]
        );
        assert!(plan::render(&plan).contains("[inputs: 1]"));
        fs::remove_dir_all(&dir).ok();
    }

    /// Writing a frozen input is refused with a structured code; waiving
    /// the item unfreezes it. New files under `tests/` stay writable.
    #[test]
    fn write_to_frozen_input_is_refused_until_waived() {
        let (mut ctx, dir) = proj();
        fs::create_dir_all(dir.join("tests")).unwrap();
        fs::write(dir.join("tests/auth.rs"), "fn t() {}\n").unwrap();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "frozen write",
                "acceptance": ["cmd: exit 3", "manual: eyeball it"],
                "steps": [{"title": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);

        // read first: the refusal must be the freeze, not the read guard
        assert!(execute(&mut ctx, "read", &json!({"file_path": "tests/auth.rs"})).ok);
        let refused = execute(
            &mut ctx,
            "edit",
            &json!({"file_path": "tests/auth.rs", "old_string": "fn t() {}", "new_string": "fn t() { assert!(true) }"}),
        );
        assert!(!refused.ok, "{}", refused.output);
        assert!(refused.output.contains("frozen_input"), "{}", refused.output);

        // `..` spellings do not dodge the freeze
        let dodged = execute(
            &mut ctx,
            "edit",
            &json!({"file_path": "src/../tests/auth.rs", "old_string": "fn t() {}", "new_string": "fn t() { assert!(true) }"}),
        );
        assert!(!dodged.ok, "{}", dodged.output);
        assert!(dodged.output.contains("frozen_input"), "{}", dodged.output);

        // new test files are always allowed (rung 2)
        let fresh = execute(
            &mut ctx,
            "write",
            &json!({"file_path": "tests/repro.rs", "content": "fn r() {}\n"}),
        );
        assert!(fresh.ok, "{}", fresh.output);

        // waiving the item takes responsibility and unfreezes its inputs
        let mut plan = plan::open_active(&dir).unwrap().unwrap();
        plan::waive(&mut plan, 0, "spec changed").unwrap();
        plan::store(&dir, &plan).unwrap();
        let allowed = execute(
            &mut ctx,
            "edit",
            &json!({"file_path": "tests/auth.rs", "old_string": "fn t() {}", "new_string": "fn t() { assert!(true) }"}),
        );
        assert!(allowed.ok, "{}", allowed.output);
        fs::remove_dir_all(&dir).ok();
    }

    /// Receipt-time guard: editing a frozen input outside the file tools
    /// (or before the guard existed) still meets the hash comparison at
    /// `verify` — plus a journal event, so the attempt is auditable.
    #[test]
    fn verify_refuses_edited_check_inputs_and_journals_it() {
        let (mut ctx, dir) = proj();
        fs::create_dir_all(dir.join("tests")).unwrap();
        fs::write(dir.join("tests/auth.rs"), "fn t() {}\n").unwrap();
        let flag = dir.join("flag.txt");
        let command = gate_probe_command(&flag);
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "edited inputs",
                "acceptance": [format!("cmd: {command}")],
                "steps": [{"title": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        // the fix happens after the baseline was taken
        fs::write(&flag, "ok").unwrap();

        // bypass the file tools straight through the filesystem
        fs::write(dir.join("tests/auth.rs"), "fn t() { assert!(false) }\n").unwrap();
        let out = plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0}));
        assert!(!out.ok, "{}", out.output);
        assert!(out.output.contains("inputs_changed"), "{}", out.output);
        assert!(out.output.contains("tests/auth.rs"), "{}", out.output);

        let records = crate::agent::journal::Journal::records(&dir).unwrap();
        assert!(
            records.iter().any(|r| {
                r.kind == "note"
                    && r.fields.get("note").and_then(|v| v.as_str()) == Some("blocker")
                    && r.fields
                        .get("text")
                        .and_then(|v| v.as_str())
                        .is_some_and(|t| t.contains("tests/auth.rs"))
            }),
            "inputs-changed journal event missing"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// Same guard at `complete`: the re-run never happens on edited inputs.
    #[test]
    fn complete_refuses_edited_check_inputs() {
        let (mut ctx, dir) = proj();
        fs::create_dir_all(dir.join("tests")).unwrap();
        fs::write(dir.join("tests/auth.rs"), "fn t() {}\n").unwrap();
        let flag = dir.join("flag.txt");
        let command = gate_probe_command(&flag);
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "edited inputs at completion",
                "acceptance": [format!("cmd: {command}")],
                "steps": [{"title": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        fs::write(&flag, "ok").unwrap();
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        assert!(plan_op(&mut ctx, &json!({"op": "verify", "acceptance": 0})).ok);

        fs::write(dir.join("tests/auth.rs"), "fn t() { assert!(false) }\n").unwrap();
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
            completed.output.contains("inputs_changed"),
            "{}",
            completed.output
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// The shell matcher is best-effort and documented as such: redirects
    /// onto frozen files ask first, reads and stream-merges never do.
    #[test]
    fn frozen_input_command_matcher() {
        let (mut ctx, dir) = proj();
        fs::create_dir_all(dir.join("tests")).unwrap();
        fs::write(dir.join("tests/auth.rs"), "fn t() {}\n").unwrap();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "matcher",
                "acceptance": ["cmd: exit 3"],
                "steps": [{"title": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let hit = |command: &str| {
            frozen_input_command_hit(&ctx.root, &ctx.session_id, command).is_some()
        };
        assert!(hit("echo x > tests/auth.rs"));
        assert!(hit("echo x >> tests/auth.rs"));
        assert!(hit("cat src/main.rs | tee tests/auth.rs"));
        assert!(!hit("cat tests/auth.rs"));
        assert!(!hit("cargo test 2>&1"));
        assert!(!hit("echo hello"));
        fs::remove_dir_all(&dir).ok();
    }

    /// `forbid-cmd:` matching is a pure substring on the lowered command:
    /// empty patterns never match, casing never matters.
    #[test]
    fn forbidden_command_matches_substrings() {
        let patterns = vec!["rm -rf".to_string(), "DROP TABLE".to_string()];
        assert_eq!(
            forbidden_command(&patterns, "rm -rf /tmp/x"),
            Some("rm -rf".to_string())
        );
        assert_eq!(
            forbidden_command(&patterns, "sudo RM -RF /"),
            Some("rm -rf".to_string())
        );
        assert_eq!(forbidden_command(&patterns, "ls -la"), None);
        assert_eq!(forbidden_command(&[], "rm -rf /"), None);
        assert_eq!(
            forbidden_command(&["  ".to_string()], "rm -rf /"),
            None,
            "blank patterns match nothing"
        );
    }

    /// Typed constraints validate at create: empty payloads, unresolvable
    /// roots, and uncompilable patterns reject while the model can rewrite.
    #[test]
    fn create_validates_typed_constraints() {
        let (mut ctx, dir) = proj();
        for (constraints, code) in [
            (vec!["ast: "], "empty_constraint"),
            (vec!["path: "], "empty_constraint"),
            (vec!["forbid-cmd: "], "empty_constraint"),
            (vec!["path: ../outside"], "bad_constraint_path"),
            (vec!["path: .sqwai/nope"], "bad_constraint_path"),
            (vec!["ast: Ok("], "bad_constraint_pattern"),
        ] {
            let rejected = plan_op(
                &mut ctx,
                &json!({
                    "op": "create",
                    "goal": "bad constraint",
                    "constraints": constraints,
                    "acceptance": ["manual: eyeball it"],
                    "steps": [{"title": "verify"}]
                }),
            );
            assert!(!rejected.ok, "{constraints:?}");
            assert!(
                rejected.output.contains(code),
                "{constraints:?}: {}",
                rejected.output
            );
        }
        fs::remove_dir_all(&dir).ok();
    }

    /// `forbid-import:` blocks `complete` naming the offending file, and a
    /// waiver with reason lets it through.
    #[test]
    fn complete_blocks_forbidden_imports_until_waived() {
        let (mut ctx, dir) = proj();
        fs::write(dir.join("src/user.rs"), "use btree::Map;\n").unwrap();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "no btree",
                "constraints": ["forbid-import: btree"],
                "acceptance": ["manual: eyeball it"],
                "steps": [{"title": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let mut plan = plan::open_active(&dir).unwrap().unwrap();
        plan::waive(&mut plan, 0, "eyeball done").unwrap();
        plan::store(&dir, &plan).unwrap();
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        assert!(
            plan_op(
                &mut ctx,
                &json!({"op": "cancel", "id": "1", "reason": "done here"})
            )
            .ok
        );

        let blocked = plan_op(&mut ctx, &json!({"op": "complete"}));
        assert!(!blocked.ok, "{}", blocked.output);
        assert!(
            blocked.output.contains("constraint_violated"),
            "{}",
            blocked.output
        );
        assert!(blocked.output.contains("src/user.rs"), "{}", blocked.output);

        let mut plan = plan::open_active(&dir).unwrap().unwrap();
        plan::waive_constraint(&mut plan, 0, "legacy use, tracked").unwrap();
        plan::store(&dir, &plan).unwrap();
        let completed = plan_op(&mut ctx, &json!({"op": "complete"}));
        assert!(completed.ok, "{}", completed.output);
        fs::remove_dir_all(&dir).ok();
    }

    /// `path:` confines the outcome diff: a recorded write outside the
    /// roots blocks completion, while writes inside pass clean.
    #[test]
    fn complete_blocks_changes_outside_pathed_roots() {
        let (mut ctx, dir) = proj();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "stay in src",
                "constraints": ["path: src"],
                "acceptance": ["manual: eyeball it"],
                "steps": [{"title": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let mut plan = plan::open_active(&dir).unwrap().unwrap();
        plan::waive(&mut plan, 0, "eyeball done").unwrap();
        plan::store(&dir, &plan).unwrap();
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        // attribute writes to the step, like the turn loop does
        ctx.current_step = Some("1".into());

        // README.md sits outside the declared roots
        assert!(execute(&mut ctx, "read", &json!({"file_path": "README.md"})).ok);
        assert!(
            execute(
                &mut ctx,
                "edit",
                &json!({"file_path": "README.md", "old_string": "# demo", "new_string": "# demo!"}),
            )
            .ok
        );
        // direct execute() calls do not journal (the turn loop records
        // outcomes); attach the file_diff the way the loop would
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        let mut journal = crate::agent::journal::Journal::open(&dir, &ctx.session_id).unwrap();
        journal.set_attribution(Some("1".into()), Some(plan_id), "main");
        journal
            .append_evidence("file_diff", json!({"path": "README.md"}))
            .unwrap();
        assert!(plan_op(&mut ctx, &json!({"op": "finish", "id": "1", "summary": "edited"})).ok);
        let blocked = plan_op(&mut ctx, &json!({"op": "complete"}));
        assert!(!blocked.ok, "{}", blocked.output);
        assert!(
            blocked.output.contains("constraint_violated"),
            "{}",
            blocked.output
        );
        assert!(blocked.output.contains("README.md"), "{}", blocked.output);

        // fresh plan, same roots, write inside: completes clean.
        // (whole-plan abandon is the user's call — the tool refuses it
        // for the model — so the test takes the user path directly)
        abandon_as_user(&ctx, &dir);
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "stay in src cleanly",
                "constraints": ["path: src"],
                "acceptance": ["manual: eyeball it"],
                "steps": [{"title": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let mut plan = plan::open_active(&dir).unwrap().unwrap();
        plan::waive(&mut plan, 0, "eyeball done").unwrap();
        plan::store(&dir, &plan).unwrap();
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        ctx.current_step = Some("1".into());
        assert!(execute(&mut ctx, "read", &json!({"file_path": "src/main.rs"})).ok);
        assert!(
            execute(
                &mut ctx,
                "edit",
                &json!({"file_path": "src/main.rs", "old_string": "TODO", "new_string": "DONE"}),
            )
            .ok
        );
        let plan_id = plan::open_active(&dir).unwrap().unwrap().id;
        let mut journal = crate::agent::journal::Journal::open(&dir, &ctx.session_id).unwrap();
        journal.set_attribution(Some("1".into()), Some(plan_id), "main");
        journal
            .append_evidence("file_diff", json!({"path": "src/main.rs"}))
            .unwrap();
        assert!(plan_op(&mut ctx, &json!({"op": "finish", "id": "1", "summary": "edited"})).ok);
        let completed = plan_op(&mut ctx, &json!({"op": "complete"}));
        assert!(completed.ok, "{}", completed.output);
        fs::remove_dir_all(&dir).ok();
    }

    /// `ast:` matches structurally: a `todo!()` in new code blocks
    /// completion until waived.
    #[test]
    fn complete_blocks_ast_matches_until_waived() {
        let (mut ctx, dir) = proj();
        fs::write(dir.join("src/extra.rs"), "fn f() { todo!() }\n").unwrap();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "no todos",
                "constraints": ["ast: todo!()"],
                "acceptance": ["manual: eyeball it"],
                "steps": [{"title": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        let mut plan = plan::open_active(&dir).unwrap().unwrap();
        plan::waive(&mut plan, 0, "eyeball done").unwrap();
        plan::store(&dir, &plan).unwrap();
        assert!(plan_op(&mut ctx, &json!({"op": "start", "id": "1"})).ok);
        assert!(
            plan_op(
                &mut ctx,
                &json!({"op": "cancel", "id": "1", "reason": "done here"})
            )
            .ok
        );

        let blocked = plan_op(&mut ctx, &json!({"op": "complete"}));
        assert!(!blocked.ok, "{}", blocked.output);
        assert!(
            blocked.output.contains("constraint_violated"),
            "{}",
            blocked.output
        );

        let mut plan = plan::open_active(&dir).unwrap().unwrap();
        plan::waive_constraint(&mut plan, 0, "will fix next").unwrap();
        plan::store(&dir, &plan).unwrap();
        assert!(plan_op(&mut ctx, &json!({"op": "complete"})).ok);
        fs::remove_dir_all(&dir).ok();
    }

    /// AGENTS.md mining is advisory: restriction markers with no typed
    /// constraint earn one note line at create.
    #[test]
    fn create_notes_unformalized_agents_restrictions() {
        let (mut ctx, dir) = proj();
        fs::write(dir.join("AGENTS.md"), "Do not use btree directly.\n").unwrap();
        let created = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "mined note",
                "constraints": ["keep the format"],
                "acceptance": ["manual: eyeball it"],
                "steps": [{"title": "verify"}]
            }),
        );
        assert!(created.ok, "{}", created.output);
        assert!(created.output.contains("forbid-import:"), "{}", created.output);

        // silent once the author formalized anything
        abandon_as_user(&ctx, &dir);
        let typed = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
                "goal": "formalized",
                "constraints": ["forbid-import: btree"],
                "acceptance": ["manual: eyeball it"],
                "steps": [{"title": "verify"}]
            }),
        );
        assert!(typed.ok, "{}", typed.output);
        assert!(
            !typed.output.contains("AGENTS.md restricts"),
            "{}",
            typed.output
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// `plan verify` on a `cmd:` item records an interval receipt: equal
    /// before/after digests, exec runner, and a matching journal record.
    ///
    /// §12.12: the probe misses before the change (baseline), is fixed,
    /// then verifies green with a receipt. `gate.txt` exists throughout
    /// so the step refs validate; the probe watches a second file, which
    /// is outside the digest — creating it is the fix, not a state move.
    #[test]
    fn verify_cmd_issues_interval_receipt() {
        let (mut ctx, dir) = proj();
        fs::write(dir.join("gate.txt"), "ok").unwrap();
        let probe = dir.join("probe-missing.txt");
        let command = gate_probe_command(&probe);
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
        fs::write(&probe, "ok").unwrap();

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
    ///
    /// §12.12: a mutating check can never take a natural baseline (the
    /// capture sees the state move and records nothing), so the proof is
    /// attached by hand to reach the runner — which must still refuse.
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

        let mut plan = plan::open_active(&dir).unwrap().unwrap();
        assert!(
            !plan::proven_failing(&plan.acceptance[0]),
            "a mutating check takes no natural baseline"
        );
        plan.acceptance[0].baseline = Some(plan::Baseline {
            at: plan::now(),
            exit: 1,
            check_definition_hash: plan::check_definition_hash("echo hi >> gate.txt"),
            output_hash: "hash".to_string(),
            head: "boom".to_string(),
            state_digest: "digest".to_string(),
        });
        plan::store(&dir, &plan).unwrap();

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
        let mut journal = crate::agent::journal::Journal::open(&dir, &ctx.session_id).unwrap();
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
            complete.output.contains("Pending acceptance items without cmd:/snapshot:/differential:/signatures: prefix require user waiver (/plan waive <index>) or conversion to steps with host evidence."),
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
    fn plan_cancel_without_id_is_refused() {
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
        let cancelled = plan_op(&mut ctx, &json!({"op": "cancel", "id": "1", "reason": "skip"}));
        assert!(cancelled.ok, "{}", cancelled.output);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn plan_cancel_with_plan_id_is_user_only() {
        let (mut ctx, dir) = proj();
        let p1 = plan_op(
            &mut ctx,
            &json!({
                "op": "create",
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
        assert!(
            pass_unknown.ok,
            "unknown must pass: {}",
            pass_unknown.output
        );

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
        assert!(
            !rej_create.ok,
            "should reject existing ref on create intent"
        );
        assert!(
            rej_create.output.contains("ref_collision"),
            "{}",
            rej_create.output
        );

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
            out.output
                .contains("warning: symbol 'dummy' not in index for this file"),
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
        let recall_out = execute(&mut ctx, "recall", &json!({"query": "persist todos"}));
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
        assert!(
            journal_out.ok,
            "journal tool must succeed: {}",
            journal_out.output
        );
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
        assert!(
            out.output.contains("unresolved_start"),
            "must contain unresolved_start code: {}",
            out.output
        );
        assert!(
            out.output.contains("hint"),
            "must contain hint: {}",
            out.output
        );

        fs::remove_dir_all(&dir).ok();
    }
}