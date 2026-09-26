//! Tool schemas, capability queries and shell classifiers.
//!
//! Moved byte-for-byte from `tools/mod.rs`; behavior unchanged.
//!
use serde_json::{Value, json};

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
command may outlast the tool timeout; wait on it with bash_output(id, wait_secs) or sleep(seconds); \
await its result before dependent changes or reporting success.",
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
            description: "Delegate one or more focused tasks to child agents. Children inherit the current Plan/Act mode; up to 8 tasks are accepted, at most 4 run concurrently, and child agents cannot create further subagents. A child silent for 600s is cancelled and reported as timed out. Children are read-only by default; a task object with write:true and paths:[...] declares a writer scoped to those roots (non-empty, non-overlapping with sibling writers) — writes outside the scope are refused.",
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
            description: "Read the host journal: the factual event log of this \
             project (user messages, tool calls and results, file diffs, plan ops, notes, \
             checkpoints). Every line carries j#<seq>, the stable reference used by plan \
             evidence and note resolves. Times are UTC. Use it to find what evidence exists \
             or what was already tried. Output is newest-first and always capped: \
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
            description: "Search the code graph by symbol name, path or concept \
using deterministic ranking. \
Returns matching items with canonical keys (sym:..., file:...), kinds, paths, one-line snippets, and provenance. \
Always prefer using the canonical keys returned by recall in subsequent graph_query calls.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "search query (symbol, path or concept)"
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
            description: "Traverse relationships in the code graph from a starting node using bounded breadth-first search. \
Accepts canonical keys (sym:..., file:...) or shorthand (path::symbol, symbol name). \
Unresolvable or ambiguous starts return an error with candidates (use recall for canonical keys). \
Default preset is 'dependencies', without expanding file containers into sibling declarations. \
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
                        "enum": ["dependencies", "structure", "all"],
                        "description": "relation preset: 'dependencies' (calls, imports, references, about; default), 'structure' (hierarchy/containment), 'all'"
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
Nothing is written until the user accepts: the host validates the draft first, \
then shows it for accept/decline with a preview. If declined, ask \
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
            name: "propose_reset",
            kind: Kind::ReadOnly,
            description: "Propose abandoning the active plan when the plan itself is wrong \
(not the work): the reason must quote the plan defect, and the user confirms \
through a dialog showing what gets discarded — nothing is written until then. \
The old plan stays on disk as abandoned; a \
replacement, if any, goes through a fresh plan create with all its gates. \
For a bad direction with a salvageable structure use propose_plan instead; \
for an impossible task use plan block_plan.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "reason": {"type": "string", "description": "quoted plan defect: which goal, constraint, or acceptance item is wrong and why"}
                },
                "required": ["reason"]
            }),
        },
        ToolDef {
            name: "plan",
            kind: Kind::Mutating,
            description: "Work the structured plan, one operation per call. Ops: create, start, \
finish, block, unblock, cancel, add, split, verify, complete, show, block_plan. To abandon \
a wrong plan, use propose_reset (user-confirmed), never cancel-and-recreate around it. Plans are for \
work that changes things: read-only inspection needs no plan — just look. \
For a write, create with goal + steps; acceptance is optional at create \
and required only as executable or human-settled criteria for \
real mutations: cmd: for a check that fails before the change and passes after, manual: for \
anything a human eyeballs. Advanced rung kinds (snapshot:/differential:/signatures:) are \
host-suggested after the first run, never written by hand. Call \
show first if you are unsure of the current step ids. The host owns the goal, the constraints, \
acceptance status, validation and evidence; to change the goal, propose the full updated plan with \
propose_plan instead. finish records completion of the step's work with a summary and does not \
by itself establish that acceptance criteria passed; rejections return a code and hint to follow. \
A manual: acceptance can be \
waived only by the user, never verified by the model. complete requires every step closed and \
every acceptance validation passed or waived with fresh receipts. block_plan surrenders \
an impossible task: use it when the spec contradicts the tests (or itself) instead of gaming \
either side — quote the conflict in reason. \
Never invent evidence identifiers.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "op": {"type": "string", "enum": [
                        "create", "start", "finish", "block", "unblock", "cancel",
                        "add", "split", "verify", "complete", "show", "block_plan",
                        "add_acceptance"
                    ]},
                    "id": {"type": "string", "description": "step id"},
                    "goal": {"type": "string", "description": "create"},
                    "constraints": {"type": "array", "items": {"type": "string"}},
                    "acceptance": {
                        "type": ["array", "integer"],
                        "items": {"type": "string"},
                        "description": "create: criteria (cmd:/manual:); verify: index"
                    },
                    "checklist": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "create: non-blocking free-text notes (never gate complete)"
                    },
                    "items": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "add_acceptance: criteria to append (cmd:/manual:; free text refused like at create)"
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

/// Multi-file (or opaque-target) mutation for the plan gate's hard path.
/// Single-file file-tool writes and bounded index ops proceed with an
/// advisory nudge instead of the plan_required refusal; anything whose
/// blast radius is unknown or spans files stays gated. Only consulted
/// when `is_mutating_call` already fired, so read-only tools never
/// reach here. Deliberately not prose-based: a short request can still
/// name a catastrophic command, so the split follows knowable blast
/// radius, never request length.
pub fn is_multi_file_mutation(name: &str, args: &Value) -> bool {
    match name {
        // one file_path each — bounded blast radius
        "write" | "edit" | "multi_edit" => false,
        // unified diff: count files, unknown or multi → hard
        "patch" => {
            let files = args
                .get("patch")
                .and_then(|value| value.as_str())
                .map(|text| {
                    text.lines()
                        .filter(|line| line.starts_with("diff --git "))
                        .count()
                });
            files != Some(1)
        }
        // bounded index ops go soft with a nudge, like a file write: an
        // explicit path list is enumerable, and a plain commit only seals
        // what is already staged (both reversible). The `all: true`
        // variants stage the whole tree — unbounded, hard.
        "git_stage" | "git_commit" => {
            args.get("all").and_then(Value::as_bool).unwrap_or(false)
        }
        // bash is opaque, git_stage/commit span the index, MCP unknown:
        // fail closed into the hard path
        _ => true,
    }
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
