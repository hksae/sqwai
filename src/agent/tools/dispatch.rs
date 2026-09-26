use super::astgrep;
use super::ctx::{MIN_PLAN_BUDGET_TOKENS, ToolCtx};
use super::exec;
use super::fs;
use super::git;
use super::outline;
use super::policy::{bash_scope_hit, commanded_verify_refs, frozen_input_hit, in_write_scope, mutation_target_paths, step_epoch_current};
use super::specs;
use super::verify::{capture_baselines, rejection, validate_complete, validate_evidence, verify_acceptance, with_assumption_warning, with_blast_radius, with_evidence_ts_warning, with_misattribution_warning};
use crate::agent::graph::GraphStore;
use crate::plan;
use serde_json::Value;
use std::path::Path;


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


const READ_MAX_BYTES: usize = 400_000;

/// (id, owning session, command) of background jobs whose processes are
/// still alive. The undo preflight (§2.5, S1) refuses a restore while any
/// of these run — the writer lock stops in-process dispatch, but an
/// already-running shell would keep writing mid-restore.
pub(crate) fn bg_running_commands() -> Vec<(u64, String, String)> {
    exec::running_commands()
}

/// End every still-running background job. Called once on app shutdown so
/// detached jobs cannot outlive the UI (and keep mutating the project).
/// Returns how many were killed.
pub(crate) fn kill_remaining_jobs() -> usize {
    exec::kill_remaining_jobs()
}

/// Project-relative, lexically cleaned mutation targets of a file-writing
/// call: `file_path` for write/edit/multi_edit, `+++` files for patch.
/// Unresolvable or empty spellings yield nothing — the tool's own
/// validation reports those, not the scope gates.

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
    {
        // `commit -a` / `git add -A` stage the whole tree: unbounded by
        // construction, so a scoped child never runs them. Plain `commit`
        // only seals what is already staged.
        if name == "git_commit" && args.get("all").and_then(Value::as_bool).unwrap_or(false) {
            return Outcome::err(
                serde_json::json!({
                    "ok": false,
                    "code": "subagent_scope",
                    "reason": "git_commit all:true stages and commits the whole tree, outside this subagent's declared write scope",
                    "hint": "stage scoped paths instead, or spawn with wider paths",
                })
                .to_string(),
            );
        }
        if name == "git_stage" && args.get("all").and_then(Value::as_bool).unwrap_or(false) {
            return Outcome::err(
                serde_json::json!({
                    "ok": false,
                    "code": "subagent_scope",
                    "reason": "git_stage all:true stages the whole tree, outside this subagent's declared write scope",
                    "hint": "stage scoped paths instead, or spawn with wider paths",
                })
                .to_string(),
            );
        }
        if matches!(
            name,
            "write" | "edit" | "multi_edit" | "patch" | "git_stage"
        ) {
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
        // Shell writes are analyzed best-effort (see `bash_scope_hit`):
        // explicit out-of-scope targets refuse, the rest runs.
        if name == "bash"
            && let Some(target) = bash_scope_hit(
                ctx,
                allowed,
                args["command"].as_str().unwrap_or_default(),
            )
        {
            return Outcome::err(
                serde_json::json!({
                    "ok": false,
                    "code": "subagent_scope",
                    "reason": format!(
                        "'{target}' is outside this subagent's declared write scope ({})",
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
    let body = specs::clip(&journal_body(r), 140);
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
                    Value::String(v) => Some(specs::clip(v, 40)),
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
pub(crate) fn plan_op(ctx: &mut ToolCtx, args: &Value) -> Outcome {
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
            checklist,
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
                let named_refs = commanded_verify_refs(&acceptance);
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
                        // non-blocking checklist rides along (never gates)
                        created.checklist = checklist;
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
                            "checklist": created.checklist,
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
                                // provenance: expanded commands come from
                                // project config, not model text — the model
                                // reviews someone else's command here
                                if !named_refs.is_empty() {
                                    message.push_str(&format!(
                                        "\nacceptance: ${} expanded from .sqwai/config.toml [verify] — project-defined commands run under the same policy as typed ones",
                                        named_refs.join(", $")
                                    ));
                                }
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
                                // acceptance suggestion: a plan born without
                                // criteria plus detected check commands gets a
                                // pointer, not a mandate — the model decides
                                // with op add_acceptance. (Read-only work
                                // rightly has none of either.)
                                if created.acceptance.is_empty() {
                                    let detected =
                                        crate::config::Config::detect_verify_commands(&ctx.root);
                                    if !detected.is_empty() {
                                        let offered: Vec<String> = detected
                                            .iter()
                                            .take(3)
                                            .map(|(name, command, _)| {
                                                format!("cmd: {command} ({name})")
                                            })
                                            .collect();
                                        message.push_str(&format!(
                                            "\nacceptance: none yet — detected checks you can adopt with {{\"op\": \"add_acceptance\", \"items\": [...]}} (or write manual:): {}",
                                            offered.join(" · ")
                                        ));
                                    }
                                }
                                // rung suggestions: a cmd: that already passes
                                // pre-change proves nothing as cmd: — offer the
                                // freeze rungs transparently; adoption goes
                                // through add_acceptance (frozen at adopt
                                // time). Capped: more than three is a lecture.
                                {
                                    let mut offered = 0;
                                    for (index, item) in created.acceptance.iter().enumerate() {
                                        if offered >= 3 {
                                            break;
                                        }
                                        let plan::AcceptanceKind::Command(command) =
                                            item.kind()
                                        else {
                                            continue;
                                        };
                                        if proof.slots.get(index).is_some_and(|slot| slot.is_some()) {
                                            continue;
                                        }
                                        // not run (unsafe/needs-approval) or
                                        // cancelled: no outcome to judge —
                                        // but a run that passed is exactly
                                        // the suggest case ("passes already")
                                        if proof.notes.iter().any(|note| {
                                            note.starts_with(&format!("\nacceptance {index}:"))
                                                && (note.contains("not run")
                                                    || note.contains("cancelled"))
                                        }) {
                                            continue;
                                        }
                                        message.push_str(&format!(
                                            "\nacceptance {index} (`cmd: {command}`) already passes pre-change, so cmd: proves nothing — to freeze this output say {{\"op\": \"add_acceptance\", \"items\": [\"snapshot: {command}\"]}} (byte-identical) or [\"differential: {command}\"] (must change)"
                                        ));
                                        offered += 1;
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
                        reason: reason.clone(),
                    };
                    match plan::apply(&mut active, op, &limits, ctx.current_step.as_deref()) {
                        Ok(applied) => {
                            // journal-first like every other mutating op: a
                            // bare store leaves crash recovery blind to the
                            // cancellation (file moved, journal did not)
                            if let Err(e) = plan::commit(
                                &ctx.root,
                                &ctx.session_id,
                                &mut active,
                                "cancel",
                                "model",
                                true,
                                serde_json::json!({"id": target_id, "reason": reason}),
                            ) {
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
        // reset needs a user confirm through the approval dialog, which only
        // the agent loop can ask for — direct application here would abandon
        // silently, which is exactly the rewrite-history hole this op closes
        plan::Op::ProposeReset { .. } => {
            return Outcome::err(
                "propose_reset is served by the agent loop, not by the dispatcher: \
                 call the propose_reset tool so the user confirms the abandon",
            )
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
            let mut other = other;
            // $named expansion for added criteria, same as create: unknown
            // names reject here, not at verify time on a command that never
            // existed.
            if let plan::Op::AddAcceptance { ref mut items } = other {
                match plan::substitute_verify_commands(
                    items.clone(),
                    &crate::config::Config::project_verify_commands(&ctx.root),
                ) {
                    Ok(expanded) => *items = expanded,
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
                }
            }
            let mut op_value = serde_json::to_value(&other).unwrap_or(serde_json::Value::Null);
            let op_name = op_value
                .get("op")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown")
                .to_string();
            let readonly_show = op_name == "show";
            // late-added criteria need baselines for exactly their new
            // positions (existing ones belong to the pre-change tree and are
            // never re-run); recorded before apply so the fill below knows
            // where the plan ended
            let prev_acceptance_len = active.acceptance.len();
            let adding_acceptance = matches!(&other, plan::Op::AddAcceptance { .. });
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
                    // baselines for late-added criteria ride the same intent
                    // (replay restores them instead of re-running checks).
                    // Captured against a plan trimmed to the new items only:
                    // re-running the old checks would waste turns and pin
                    // notes to stale indices. A check that already passes
                    // stays unproven — correctly, the host never saw it fail.
                    let mut proof_notes = String::new();
                    if adding_acceptance && active.acceptance.len() > prev_acceptance_len {
                        let mut probe = active.clone();
                        probe.acceptance = probe.acceptance.split_off(prev_acceptance_len);
                        let proof = capture_baselines(ctx, &probe);
                        let proven = proof.slots.iter().filter(|slot| slot.is_some()).count();
                        let added = active.acceptance.len() - prev_acceptance_len;
                        for (offset, item) in active
                            .acceptance
                            .iter_mut()
                            .enumerate()
                            .skip(prev_acceptance_len)
                        {
                            let slot = offset - prev_acceptance_len;
                            item.baseline = proof.slots.get(slot).cloned().flatten();
                            item.snapshot = proof.frozen.get(slot).cloned().flatten();
                            item.shape = proof.shapes.get(slot).cloned().flatten();
                            item.inputs =
                                proof.inputs.get(slot).cloned().unwrap_or_default();
                        }
                        if let Some(record) = op_value.as_object_mut() {
                            record.insert(
                                "new_baselines".to_string(),
                                serde_json::to_value(&proof.slots).unwrap_or_default(),
                            );
                            record.insert(
                                "new_snapshots".to_string(),
                                serde_json::to_value(&proof.frozen).unwrap_or_default(),
                            );
                            record.insert(
                                "new_shapes".to_string(),
                                serde_json::to_value(&proof.shapes).unwrap_or_default(),
                            );
                            record.insert(
                                "new_inputs".to_string(),
                                serde_json::to_value(&proof.inputs).unwrap_or_default(),
                            );
                        }
                        proof_notes.push_str(&format!(
                            "\nbaselines: {proven}/{added} new items fail pre-change and can settle; \
                             the rest already pass and stay unproven (waivable)"
                        ));
                    }
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
                            let msg = with_blast_radius(
                                ctx,
                                finishing.as_deref(),
                                &active,
                                msg,
                            );
                            let msg = format!("{msg}{proof_notes}");
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

