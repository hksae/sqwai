//! Tool schemas, capability queries and shell classifiers.
//!
//! Moved byte-for-byte from `tools/mod.rs`; behavior unchanged.
//!
use serde_json::{Value, json};

use crate::agent::loop_task::{ASK_MAX_OPTIONS, ASK_MAX_QUESTIONS, ASK_MIN_OPTIONS};

/// Clip to `max` chars on a char boundary (… marks the cut).
pub(crate) fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let taken: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{taken}…")
}

/// whether a tool may run in parallel with others
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// pure: never mutates the worktree
    ReadOnly,
    /// mutates files or runs processes; runs alone, gets a checkpoint
    Mutating,
}

struct ToolDef {
    name: &'static str,
    kind: Kind,
    description: &'static str,
    parameters: Value,
}

/// Shell syntax guidance, built from the shell the host will actually spawn.
///
/// The text used to hardcode "On Windows the shell is cmd.exe/PowerShell … use
/// `| Select-Object -First/Last N`". A live session on Windows obeyed it, ran
/// under cmd.exe where no such cmdlet exists, and lost the call to exit 255 —
/// the tool description was the bug. `ShellKind::detect()` already decides which
/// classifier layer is authoritative, so the same answer goes here.
fn bash_description() -> &'static str {
    static TEXT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TEXT.get_or_init(|| {
        let guidance = match crate::agent::shell::ShellKind::detect() {
            crate::agent::shell::ShellKind::PowerShell => {
                "The shell here is PowerShell: no POSIX coreutils — do not use it to read or \
search files (`cat`, `sed`, `grep`, `Select-String`); use the dedicated `read`, `grep`, `glob`, \
and `outline` tools instead."
            }
            crate::agent::shell::ShellKind::Cmd => {
                "The shell here is cmd.exe: no POSIX coreutils and no PowerShell cmdlets — \
do not use it to read or search files; use the dedicated `read`, `grep`, `glob`, and `outline` \
tools instead."
            }
            crate::agent::shell::ShellKind::Bash | crate::agent::shell::ShellKind::Sh => {
                "The shell here is bash: pipes work, and PowerShell cmdlets like `Select-Object` \
do not. Do not use `bash` (`sed`, `cat`, `head`, `tail`, `grep`, `find`) to read or search files \
— use the dedicated `read`, `grep`, `glob`, and `outline` tools instead."
            }
        };
        format!(
            "Run a shell command in the project directory — the way to build, test, \
install and inspect anything the file tools cannot reach. Destructive or risky shapes (rm -rf, \
sudo, disk ops, force-push, sending data outward) stop at the classifier for one user approval: \
that is the seatbelt doing its job, so name what the command does rather than reaching around it. \
{guidance} \
Long output is truncated to a tail and the full log path is returned. Use background=true when the \
command may outlast the tool timeout; wait on it with bash_output(id, wait_secs) or sleep(seconds); \
await its result before dependent changes or reporting success."
        )
    })
}

fn defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "read",
            kind: Kind::ReadOnly,
            description: "Read a file from the project. Returns numbered lines \
with totals (use offset/limit windows of 200–500 lines rather than tiny 30–50 \
line slices across multiple calls; truncated output spills \
to a file — grep it instead of re-reading). Must be called once before \
replace_all edits and overwrites of existing files — one read per session \
is enough: the host tracks freshness and refuses the edit with a stale \
warning if the file moved underneath, so never re-read defensively. \
A first read of a large file carries its structural outline, so read windows \
of it deliberately rather than scanning line by line; a window already shown \
to you is answered with a pointer, not the bytes again.",
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
Overwriting an existing file requires a prior read (one per session is enough unless the host \
reported the file stale); new files need none.",
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
unless replace_all is true. A single exact edit needs no prior read (it fails safe on \
mismatch); replace_all needs one. Never edit on a stale read — the host refuses.",
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
            description: "Apply multiple exact replacements to one file atomically in a single call. Prefer this over sequential `edit` calls when changing several places in the same file. Exact single matches check freshness automatically; any entry with `replace_all: true` requires a prior `read` of the file.",
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
            description: "List one directory's entries (name, type, size). First move in an unfamiliar tree, and the check on what a write actually created.",
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}}
            }),
        },
        ToolDef {
            name: "glob",
            kind: Kind::ReadOnly,
            description: "Find files by name when you know its shape but not its place: src/**/*.rs, **/*test*.py. Respects .gitignore.",
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
            description: "Regex search over file contents. Returns file:line: text matches. \
Pass context:N for N surrounding lines per match (like grep -C) so a follow-up read is rarely needed.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "description": "dir or file to search"},
                    "include": {"type": "string", "description": "filename glob filter, e.g. *.rs"},
                    "context": {"type": "integer", "description": "surrounding lines per match, like grep -C (default 0)"}
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
            description: bash_description(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "the shell command line to run"},
                    "timeout": {"type": "integer", "description": "seconds; kills the process on expiry"},
                    "background": {"type": "boolean", "description": "detach and return a job id immediately; monitor with bash_output(id, wait_secs)"}
                },
                "required": ["command"]
            }),
        },
        ToolDef {
            name: "bash_output",
            kind: Kind::ReadOnly,
            description: "Wait for a background command and read its output. ALWAYS prefer wait_secs (0-60): it parks until the job exits or the timeout lapses, then returns everything accumulated. Reads without it on a running job are free twice, then force-waited (15s, then 30s). Output is incremental (first read: tail; later: only new bytes; from_start=true re-reads the tail). Only this session's jobs are visible. A finished job is reported once with its exit code, then cleaned up. Without id: list this session's background jobs.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "job id from bash background=true"},
                    "tail": {"type": "integer", "description": "bytes of output to return (default 10000, max 50000)"},
                        "wait_secs": {"type": "integer", "description": "PREFERRED: park up to N seconds (max 60) until the job exits"},
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
            name: "git_status",
            kind: Kind::ReadOnly,
            description: "Show the Git branch and worktree status. Use it before committing and after a run that touched many files — it names what moved, which a count does not.",
            parameters: json!({"type":"object","properties":{"porcelain":{"type":"boolean"}}}),
        },
        ToolDef {
            name: "git_diff",
            kind: Kind::ReadOnly,
            description: "Show the unstaged diff, optionally one path. Read your own edits back before reporting them.",
            parameters: json!({"type":"object","properties":{"target":{"type":"string"}}}),
        },
        ToolDef {
            name: "git_log",
            kind: Kind::ReadOnly,
            description: "Show recent commits. Use it to find where a behavior came from before blaming the tree.",
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
            description: "Create a commit from staged changes, or from all tracked changes when `all` is true. Use only when requested by the user or required by project instructions (AGENTS.md / SQWAI.md).",
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
            description: "Multi-source web & developer meta-search (Google/Startpage, Bing, DuckDuckGo, StackOverflow, GitHub, crates.io, npm, HackerNews, Wikipedia, ArXiv) fused via Reciprocal Rank Fusion and BM25. By default (`deep: true`) also fetches the top results in parallel, caches them locally, and returns the most relevant Markdown sections and code blocks in a single call.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "search query"},
                    "category": {
                        "type": "string",
                        "enum": ["auto", "docs", "code", "packages", "discussions", "papers"],
                        "description": "search focus: 'auto' (default: web + StackOverflow + GitHub + packages), 'docs' (official docs + docs.rs), 'code' (GitHub repos/issues + StackOverflow), 'packages' (crates.io + npm + PyPI), 'discussions' (HN + StackOverflow + GitHub issues), 'papers' (ArXiv + Wikipedia)"
                    },
                    "deep": {
                        "type": "boolean",
                        "description": "when true (default), automatically fetches and extracts relevant Markdown & code sections from the top results"
                    },
                    "count": {"type": "integer", "minimum": 1, "maximum": 10},
                    "timeout": {"type": "integer", "minimum": 1, "maximum": 60}
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "webfetch",
            kind: Kind::ReadOnly,
            description: "Fetch an HTTP(S) URL and convert it into structured Markdown (preserving headings, fenced code blocks, tables, and links) with native adapters for GitHub (files, repos, issues, PRs), StackOverflow, crates.io, npm, PyPI, Reddit, Wikipedia, and `/llms.txt` v2. Pages are cached locally by section: pass `query` to rank and extract the most relevant sections & code blocks via BM25, or `section` to read a specific heading from the page outline.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "query": {
                        "type": "string",
                        "description": "optional search keywords to rank and extract the most relevant sections and code blocks from the page"
                    },
                    "section": {
                        "type": "string",
                        "description": "optional heading name from the page's Document Outline to read a specific section (served instantly from cache)"
                    },
                    "selector": {
                        "type": "string",
                        "description": "optional CSS selector to extract only matching HTML elements"
                    },
                    "timeout": {"type": "integer", "minimum": 1, "maximum": 60}
                },
                "required": ["url"]
            }),
        },
        ToolDef {
            name: "subagent",
            kind: Kind::ReadOnly,
            description: "Delegate 1–8 focused tasks to isolated child agents (up to 4 run concurrently). Children inherit the cached project/system prefix and receive a host-generated `<parent_context>` brief (active plan goal/step, parent's recent files and notes) plus a `<subagent_contract>` tailored to their mode: Read-Only Explorer (`write: false`, default) for broad multi-file research without polluting parent context, or Scoped Writer (`write: true, paths: [...]`, non-overlapping across siblings) for isolated implementation. Pass optional `files` or `context` in task objects to point children at known starting points. Returns each child's report with a host-verified `[Subagent receipt: ...]` of executed tools and modified files.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "items": {
                            "anyOf": [
                                {"type": "string"},
                                {
                                    "type": "object",
                                    "properties": {
                                        "task": {"type": "string"},
                                        "prompt": {"type": "string"},
                                        "description": {"type": "string"},
                                        "context": {"type": "string", "description": "optional background/decisions from parent to guide the child"},
                                        "files": {"type": "array", "items": {"type": "string"}, "description": "optional starting file/dir paths to inspect"},
                                        "write": {"type": "boolean", "description": "allow file writes, scoped to paths"},
                                        "paths": {"type": "array", "items": {"type": "string"}, "description": "write scope roots (required with write:true; or focus paths when write:false)"}
                                    },
                                    "additionalProperties": true
                                }
                            ]
                        },
                        "minItems": 1,
                        "maxItems": 8,
                        "description": "focused child tasks to run concurrently: strings, or objects with task|prompt"
                    }
                },
                "required": ["tasks"]
            }),
        },
        ToolDef {
            name: "note",
            kind: Kind::ReadOnly,
            description: "Record a concise note in the host journal — the durable event log \
that survives compaction and feeds the next session's anchor. Use it at the close of a \
step to capture what context will lose: a `decision` you made and why, a `rejected` \
approach (so it is not retried), an `assumption` you are relying on but have not verified \
(pass `resolves` with that note's seq to close it later), a `lesson` learned, or a `blocker`. \
Notes are how your reasoning outlives the transcript, and tomorrow-you reads them blind. \
One batch at the close of a step is enough.",
            parameters: json!({"type":"object","properties":{"note":{"type":"string"},"kind":{"type":"string","enum":["decision","rejected","assumption","lesson","blocker"]},"resolves":{"type":"integer","description":"journal seq of an assumption this note closes"}},"required":["note","kind"]}),
        },
        ToolDef {
            name: "journal",
            kind: Kind::ReadOnly,
            description: "Read the host journal: the factual event log of this \
             project (user messages, tool calls and results, file diffs, plan ops, notes, \
             checkpoints). Every line carries j#<seq>, the stable reference used by plan \
             evidence and note resolves. Times are UTC. Use it to find what evidence exists \
             or what was already tried; for what happened recently in this project — including \
             sessions other than this one — use op=recap, which summarizes each of the last days' \
             sessions (plan and its status, files touched, call counts, open threads) from host \
             records alone. Output is newest-first and always capped: \
             narrow with kind/step/from/to/after/query instead of dumping everything. \
             Ground truth for anything that already happened: your context memory is \
             lossy (early history gets compacted away), the journal is not. Before saying \
             what was changed, moved or deleted, query it (query=<filename> usually finds it \
             in one call) and report what you found — including when it says you were wrong.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "op": {"type": "string", "enum": ["read", "recap", "assumptions"], "description": "read journal records (default); recap = what happened in this project over the last days, per session; assumptions = list open ones"},
                    "days": {"type": "integer", "description": "recap only: how many days back, today included (default 3, max 7)"},
                    "session": {"type": "string", "description": "whose journal to read: 'current' (default), 'all' (every session in the project), or a session id / prefix from op=recap"},
                    "kind": {"type": "string", "description": "exact record kind: user_msg|tool_call|tool_result|file_diff|diagnostics|note|plan|checkpoint|undo|redo|memory|provider_error|compaction"},
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
            name: "recall_result",
            kind: Kind::ReadOnly,
            description: "Return the full stored output of one earlier tool call, by the \
`call_id` a cleared result names in its place. Old tool output is trimmed from the request, \
never from the session — so this is how you get it back without running the tool again: a \
re-read sees whatever the file became since, and a re-run of a mutating command is work you \
did not plan for. Refused when the result is still in your context (it is above you) and \
when compaction has dropped it from the transcript.",
            parameters: json!({"type":"object","properties":{"call_id":{"type":"string","description":"the call id the cleared result names"}},"required":["call_id"]}),
        },
        ToolDef {
            name: "memory_write",
            kind: Kind::ReadOnly,
            description: "Persist one durable fact to host memory (MEMORY.md for the \
project, USER.md for the user) — it is injected into your context at the start of \
every future session, so this is how knowledge outlives the conversation. Write it \
as soon as you learn something that will still matter later: a stated user preference \
or an agreement, a project convention, a decision or fact that is not derivable from \
code or git, an environment gotcha. Use `replaces` with the old text to update a fact \
in place instead of duplicating it, or pass `replaces` with `text: \"\"` to delete an \
outdated entry. Store durable truths only — not transient task state, \
guesses, or anything you would not want shown to you every session.",
            parameters: json!({"type":"object","properties":{"section":{"type":"string","enum":["Project","Conventions","User","Agreements"]},"scope":{"type":"string","enum":["project","user"]},"text":{"type":"string","description":"new fact to record, or empty string when using `replaces` to delete an outdated entry"},"replaces":{"type":"string","description":"existing fact text to update in place (or delete when `text` is empty)"}},"required":["section","text"]}),
        },
        ToolDef {
            name: "plan",
            kind: Kind::Mutating,
            description: "Work the structured plan, one operation per call. Ops: create, start, \
finish, block, unblock, cancel, add, split, add_criteria, step_diff, complete, block_plan, abandon. To retire \
a wrong plan, use op abandon with a reason (it is for wrong plans, not hard steps — a stuck step is \
block or ask_user), never cancel-and-recreate around it. Plans are for \
multi-step work that changes things: read-only inspection or a trivial one-line edit needs no plan. \
For a multi-step change, create with goal + steps + at least one criteria note. criteria are your own \
plain-text done-notes: what must be true when the work is done. They survive compaction and \
keep you honest with yourself — the host runs nothing and certifies nothing, so write each one \
as something you can check yourself (run the command with bash, read its observed result). \
add_criteria appends more notes as the work reveals them. The current plan, \
with its step ids, is always in your context above — use those ids, never \
guess them and never re-read the plan through tools. The host owns the goal and the constraints; \
changing the goal is the user's call, so if the direction has moved on, abandon the plan (op \
abandon with a reason) and create a fresh one. finish records \
completion of the step's work with a summary of what changed and any remaining limitations. \
complete requires every step closed (done, cancelled or blocked with reason). block_plan surrenders \
an impossible task: use it when the spec contradicts the tests (or itself) instead of gaming \
either side — quote the conflict in reason.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "op": {"type": "string", "enum": [
                        "create", "start", "finish", "block", "unblock", "cancel",
                        "add", "split", "complete", "block_plan", "abandon",
                        "add_criteria", "step_diff"
                    ]},
                    "id": {"type": "string", "description": "step id"},
                    "step_id": {"type": "string", "description": "step_diff: plan step id"},
                    "path": {"type": "string", "description": "step_diff: restrict the diff to this path"},
                    "goal": {"type": "string", "description": "create"},
                    "constraints": {"type": "array", "items": {"type": "string"}},
                    "criteria": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "create / add_criteria: plain-text done-notes of what must be true when finished"
                    },
                    "steps": {
                        "type": "array",
                        "description": "create: initial steps (3-12)",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": {"type": "string"}
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
                                "title": {"type": "string"}
                            },
                            "required": ["title"]
                        }
                    },
                    "after": {"type": "string", "description": "add: insert after this step id"},
                    "title": {"type": "string", "description": "add: new step title"},
                    "summary": {"type": "string", "description": "finish: what was done, where, and any remaining limitations"},
                    "reason": {"type": "string", "description": "block / cancel / block_plan: the quoted conflict for block_plan"},
                    "confirm": {"type": "boolean", "description": "start: re-read a stale step"}
                },
                "required": ["op"]
            }),
        },
        ToolDef {
            name: "ask_user",
            kind: Kind::ReadOnly,
            description: "Ask the user structured questions (1-4) with 2-5 answer options each. Use only for decisions that materially change the outcome (approach, library, schema), never for trivial clarification. The user can pick options (single or multiple per question) and/or type a custom answer per question. Evidence before escalation: never ask the user to resolve a question of fact about the past (what changed, who deleted what) — query the journal first; ask only when the journal cannot answer.",
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
                                    "minItems": ASK_MIN_OPTIONS,
                                    "maxItems": ASK_MAX_OPTIONS
                                },
                                "multiple": {"type": "boolean", "description": "Allow selecting multiple choices"},
                                "allow_free": {"type": "boolean", "description": "Allow a custom typed answer as an extra option"}
                            },
                            "required": ["question", "options"]
                        },
                        "minItems": 1,
                        "maxItems": ASK_MAX_QUESTIONS
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
                        "minItems": ASK_MIN_OPTIONS,
                        "maxItems": ASK_MAX_OPTIONS
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

/// full call arguments for the journal (forensics): same object the model
/// sent, with long strings cut to a head so a `write` body cannot bloat
/// the journal. `args_digest` stays the human skim; this is the audit trail
/// (e.g. whether a `read` used offset/limit windows or re-read the file).
pub fn call_args_for_journal(args: &Value) -> Value {
    const MAX_STR: usize = 500;
    fn cut(value: &Value) -> Value {
        match value {
            Value::String(s) if s.chars().count() > MAX_STR => {
                let head: String = s.chars().take(MAX_STR).collect();
                Value::String(format!("{head}…({} chars total)", s.chars().count()))
            }
            Value::Array(items) => Value::Array(items.iter().map(cut).collect()),
            Value::Object(map) => {
                Value::Object(map.iter().map(|(k, v)| (k.clone(), cut(v))).collect())
            }
            other => other.clone(),
        }
    }
    cut(args)
}

/// one-line description of a call's arguments for the live TUI row
pub fn call_summary(name: &str, args: &Value) -> String {
    let raw = call_summary_raw(name, args);
    if raw.contains(['\n', '\r', '\t']) {
        raw.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        raw
    }
}

fn call_summary_raw(name: &str, args: &Value) -> String {
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
        "outline" => {
            let path = s("path");
            if let Some(d) = args["max_depth"].as_u64() {
                format!("{path} depth={d}")
            } else {
                path
            }
        }
        "git_diff" => s("target"),
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
        "memory_write" => format!("{}: {}", s("scope"), s("text")),
        "ask_user" => {
            let single = s("question");
            if !single.is_empty() {
                return single;
            }
            // multi-question mode: compact one-line summary for the chat row,
            // live and restored alike (the ask body is this row's expansion)
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

pub fn tool_specs(_plan_mode: bool) -> Vec<crate::providers::ToolSpec> {
    // One schema set in every mode. The tool block is part of the request
    // prefix, so a mode-dependent set re-keys the cache on every Plan/Act
    // toggle; masking (Manus-style) would cost the same. Plan mode is
    // enforced by the dispatcher instead (`is_mutating_call` at dispatch
    // refuses mutating calls with an honest message, `git_branch` actions
    // included) — which is where the authority already lived.
    let mut specs: Vec<crate::providers::ToolSpec> = defs()
        .into_iter()
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
            '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐',
        ][(byte - 0xB0) as usize],
        0xC0..=0xCF => [
            '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧',
        ][(byte - 0xC0) as usize],
        0xD0..=0xDF => [
            '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '■',
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

/// The plan tool's criteria are the agent's own plain-text done-notes: the
/// description must present them as notes the host never runs or certifies.
#[test]
fn plan_tool_description_describes_criteria_as_notes() {
    let plan = tool_specs(false)
        .into_iter()
        .find(|s| s.name == "plan")
        .expect("plan tool");
    assert!(
        plan.description.contains("criteria"),
        "plan description must cover criteria: {}",
        &plan.description[..plan.description.len().min(200)]
    );
    assert!(
        plan.description.contains("host runs nothing"),
        "plan description must rule out host verification: {}",
        &plan.description[..plan.description.len().min(200)]
    );
}

/// Decode check: cp866 "Привет" (П=0x8F, р=0xE0, и=0xA8, в=0xA2,
/// е=0xA5, т=0xE2) must round-trip, valid UTF-8 (emoji included) passes
/// through untouched, and mixed lines decode each in its own encoding.
#[test]
fn decode_child_output_handles_console_codepage() {
    assert_eq!(
        decode_child_output(&[0x8F, 0xE0, 0xA8, 0xA2, 0xA5, 0xE2]),
        "Привет"
    );
    assert_eq!(decode_child_output("ok 🔥 ЕС".as_bytes()), "ok 🔥 ЕС");
    let mut mixed = b"done\n".to_vec();
    mixed.extend_from_slice(&[0x8E, 0xE8, 0xA8, 0xA1, 0xAA, 0xA0]); // "Ошибка" in cp866
    assert_eq!(decode_child_output(&mixed), "done\nОшибка");
    assert_eq!(decode_child_output(&[0xB3]), "│");
}

#[test]
fn call_args_for_journal_cuts_long_strings() {
    let args = serde_json::json!({
        "file_path": "a.txt",
        "content": "x".repeat(600),
        "offset": 3,
    });
    let kept = call_args_for_journal(&args);
    assert_eq!(kept["file_path"], serde_json::json!("a.txt"));
    assert_eq!(kept["offset"], serde_json::json!(3));
    let body = kept["content"].as_str().unwrap();
    assert!(body.contains("chars total"), "{body}");
    assert!(body.len() < 600);
}

/// The schema is what the model is told; the loop's guard reads the same
/// constants, so the two cannot disagree. The description is the one place the
/// numbers are still written by hand, so it is pinned here too — change a
/// constant and this test names the sentence to update.
#[test]
fn the_ask_user_schema_matches_the_shared_bounds() {
    use crate::agent::loop_task::{ASK_MAX_OPTIONS, ASK_MAX_QUESTIONS, ASK_MIN_OPTIONS};
    let spec = tool_specs(false)
        .into_iter()
        .find(|t| t.name == "ask_user")
        .expect("ask_user is offered");
    let n = |v: usize| serde_json::Value::from(v);
    let props = &spec.parameters["properties"];
    assert_eq!(props["questions"]["maxItems"], n(ASK_MAX_QUESTIONS));
    assert_eq!(
        props["questions"]["items"]["properties"]["options"]["minItems"],
        n(ASK_MIN_OPTIONS)
    );
    assert_eq!(
        props["questions"]["items"]["properties"]["options"]["maxItems"],
        n(ASK_MAX_OPTIONS)
    );
    assert_eq!(props["options"]["minItems"], n(ASK_MIN_OPTIONS));
    assert_eq!(props["options"]["maxItems"], n(ASK_MAX_OPTIONS));
    assert!(
        spec.description
            .contains(&format!("(1-{ASK_MAX_QUESTIONS})"))
            && spec
                .description
                .contains(&format!("{ASK_MIN_OPTIONS}-{ASK_MAX_OPTIONS}")),
        "the description still names other bounds: {}",
        spec.description
    );
}

/// Wire weight of the tool surface: every request carries all specs, so
/// this number is the static per-request overhead. Pinned so schema
/// growth stays a conscious decision, not drift.
#[test]
fn tool_specs_wire_weight() {
    let specs = tool_specs(false);
    let wire: String = specs
        .iter()
        .map(|s| serde_json::to_string(&s.parameters).unwrap_or_default())
        .collect();
    let bytes = wire.len() + specs.iter().map(|s| s.description.len()).sum::<usize>();
    eprintln!(
        "tool specs wire weight: {bytes} bytes across {} tools",
        specs.len()
    );
    assert!(
        bytes < 120_000,
        "tool surface grew past budget: {bytes} bytes"
    );
}

/// #185: the diagnostics loop reads its targets from what a call reported
/// touching, so the registry of mutating tools has to stay classified: either a
/// tool reports a per-call path (the loop can lint it), or it is listed here as
/// having none. A new mutating tool that lands in neither bucket fails this
/// test on purpose — the bug being fixed was `patch` changing files while the
/// gate knew only three names.
#[test]
fn every_mutating_tool_is_classified_for_diagnostics() {
    const REPORTS_A_PATH: &[&str] = &["write", "edit", "multi_edit", "patch"];
    const NO_PER_CALL_PATH: &[&str] = &[
        "bash",       // targets unknown in advance; attributed via the shadow head (H4)
        "git_commit", // records the tree, does not edit it
        "git_stage",  // touches the index, not file bytes
        "plan",       // host state under .sqwai/, not a source file
    ];

    let mut classified = 0usize;
    for name in tool_names() {
        if !is_mutating(&name) {
            continue;
        }
        classified += 1;
        assert!(
            REPORTS_A_PATH.contains(&name.as_str()) || NO_PER_CALL_PATH.contains(&name.as_str()),
            "{name} mutates but is not classified for diagnostics: report a FileDiff from it, \
             or add it to NO_PER_CALL_PATH with the reason it has no lintable path"
        );
    }
    assert_eq!(
        classified,
        REPORTS_A_PATH.len() + NO_PER_CALL_PATH.len(),
        "the classified lists and the tool registry drifted: a mutating tool was added to one \
         place and not the other"
    );
}
