use super::ctx::{MIN_PLAN_BUDGET_TOKENS, ToolCtx};
use super::exec;
use super::fs;
use super::git;
use super::outline;
use super::policy::{bash_scope_hit, in_write_scope, mutation_target_paths, step_epoch_current};
use super::specs;
use super::verify::{
    rejection, with_assumption_warning, with_blast_radius, with_evidence_ts_warning,
};
use crate::plan;
use serde_json::Value;

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
            && let Some(target) =
                bash_scope_hit(ctx, allowed, args["command"].as_str().unwrap_or_default())
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
            args["context"].as_u64().unwrap_or(0).min(10) as usize,
        ),
        "git_status" => git::status(ctx, args),
        "git_diff" => git::diff(ctx, args),
        "git_log" => git::log(ctx, args),
        "git_show" => git::show(ctx, args),
        "git_commit" => git::commit(ctx, args),
        "git_stage" => git::stage(ctx, args),
        "git_branch" => git::branch(ctx, args),
        "patch" => git::patch(ctx, args),
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
        "plan" => plan_op(ctx, args),
        "memory_read" => match crate::agent::diary::read_day(
            &ctx.root,
            args["date"].as_str().unwrap_or_default(),
        ) {
            Ok(text) => Outcome::ok(text),
            Err(message) => Outcome::err(message),
        },
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
                        "j#{resolves} is not an open assumption — note without \
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

/// The `plan` tool: one operation per call, validated by the host (§2.1.3).
pub(crate) fn plan_op(ctx: &mut ToolCtx, args: &Value) -> Outcome {
    // verify was retired: the host no longer certifies checks. Catch it by
    // name before parsing so the model gets the reason, not a schema error.
    if args.get("op").and_then(|v| v.as_str()) == Some("verify") {
        return Outcome::err(
            "verify is retired: run the command yourself (bash) and work from its observed result",
        );
    }
    let op: plan::Op = match serde_json::from_value(args.clone()) {
        Ok(op) => op,
        Err(e) => {
            return Outcome::err(format!(
                "plan op rejected: {e} — the current plan with its valid ops is in your context above"
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
    // experiment: `show` is not a model tool — the current plan always
    // rides in the request context, there is nothing to re-read. Kept in
    // the schema types for compat; the model must never see it succeed.
    if matches!(op, plan::Op::Show) {
        return Outcome::err(
            "no plan show tool: the current plan with its step ids is in your context above — use those ids, never re-read the plan",
        );
    }
    // Read-only step diffs bypass validator, journal and store: nothing is
    // recorded, like Show (which keeps its own path below).
    if let plan::Op::StepDiff { step_id, path } = &op {
        return match plan::step_diff(
            &ctx.root,
            &ctx.session_id,
            ctx.shadow_store,
            ctx.checkpoint_chain(),
            step_id,
            path.as_deref(),
        ) {
            Ok(text) => Outcome::ok(text),
            Err(r) => rejection(r),
        };
    }
    let limits = plan::Limits {
        max_steps: ctx.plan_limits.max_steps,
    };

    match op {
        plan::Op::Create {
            goal,
            constraints,
            criteria,
            steps,
        } => match plan::open_active_for_session(&ctx.root, Some(&ctx.session_id)) {
            Ok(Some(existing)) => rejection(plan::Rejection {
                code: "plan_exists",
                reason: format!("an active plan already exists: {}", existing.id),
                hint: "work that plan instead: start/finish its steps, do not create another one. The current plan is in your context above".to_string(),
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
                match plan::create(goal, constraints, criteria, steps, budget_limit, &limits) {
                    Ok(mut created) => {
                        created.sessions = vec![ctx.session_id.clone()];
                        let id = created.id.clone();
                        let step_count = created.steps.len();
                        // Journal-first (§2.1.4): the intent carries everything
                        // replay needs to rebuild this plan.
                        let args = serde_json::json!({
                            "goal": created.goal.text,
                            "constraints": created.constraints,
                            "criteria": created.criteria,
                            "steps": created.steps.iter().map(|s| serde_json::json!({
                                "title": s.title,
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
        plan::Op::ProposeReset { .. } => Outcome::err(
            "propose_reset is served by the agent loop, not by the dispatcher: \
                 call the propose_reset tool so the user confirms the abandon",
        ),
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
                            let msg = with_blast_radius(ctx, finishing.as_deref(), &active, msg);
                            Outcome::ok(msg)
                        }
                        plan::Applied::Shown { text } => Outcome::ok(text),
                        plan::Applied::Completed => {
                            let mut msg = format!("plan {} completed", active.id);
                            // soft nudge (§8): a finished plan is a natural moment
                            // to persist durable facts — but only nag while project
                            // memory is still empty, so it never repeats on every plan.
                            let memory_empty =
                                std::fs::read_to_string(crate::agent::memory::project_path(
                                    &ctx.root,
                                ))
                                .map(|text| {
                                    !text
                                        .lines()
                                        .any(|line| line.trim_start().starts_with("- "))
                                })
                                .unwrap_or(true);
                            if memory_empty {
                                msg.push_str(
                                    " — if this run surfaced a durable fact (convention, \
                                     decision, gotcha), memory_write it before context trims.",
                                );
                            }
                            Outcome::ok(msg)
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
