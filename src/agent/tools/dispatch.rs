use super::ctx::ToolCtx;
use super::exec;
use super::fs;
use super::git;
use super::outline;
use super::policy::{bash_scope_hit, in_write_scope, mutation_target_paths, step_epoch_current};
use super::specs;
use super::verify::{rejection, with_authorship, with_blast_radius};
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
    /// blake3 name of the pre-image in the layer-1 blob store, when it
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
    /// the user pressed Esc while this tool was running. Distinct from
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
    /// `tool_result ok:false code:cancelled`. The step stays
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

/// (id, owning session, command) of background jobs whose processes are
/// still alive. The undo preflight refuses a restore while any
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
    // would attach stale work to a fresh epoch. Refuse instead.
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
        "git_status" => with_authorship(ctx, git::status(ctx, args)),
        "git_diff" => with_authorship(ctx, git::diff(ctx, args)),
        "git_log" => git::log(ctx, args),
        "git_show" => git::show(ctx, args),
        "git_commit" => git::commit(ctx, args),
        "git_stage" => git::stage(ctx, args),
        "git_branch" => git::branch(ctx, args),
        "patch" => git::patch(ctx, args),
        "outline" => outline::outline(ctx, args),
        "webfetch" | "websearch" => Outcome::err("web tools must run through the async dispatcher"),
        "bash" => exec::bash_with_workdir(
            ctx,
            args["command"].as_str().unwrap_or_default(),
            args["timeout"].as_u64(),
            args["background"].as_bool().unwrap_or(false),
            args["workdir"]
                .as_str()
                .or_else(|| args["cwd"].as_str()),
        ),
        "bash_output" => exec::bash_output(ctx, args),
        "bash_kill" => exec::bash_kill(ctx, args),
        "sleep" => exec::sleep(ctx, args),
        "plan" => plan_op(ctx, args),
        "note" => {
            let note = args
                .get("note")
                .or_else(|| args.get("text"))
                .or_else(|| args.get("message"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim();
            let kind = args
                .get("kind")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("decision");
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
        "recall_result" => {
            Outcome::err("recall_result is served by the agent loop, not by the dispatcher")
        }
        "journal" => journal_op(ctx, args),
        other => Outcome::err(format!("unknown tool '{other}'")),
    }
}

/// The `journal` tool: a read-only projection of the host journal.
///
/// The model never writes here except through `note`, and it cannot read the
/// journal files directly (host-owned state), so this op is the one window on
/// past actions. Everything it returns was screened when it was appended; the
/// output is filtered and capped so a broad query cannot flood the context.
fn journal_op(ctx: &mut ToolCtx, args: &Value) -> Outcome {
    let op = args["op"].as_str().unwrap_or("read");
    if op == "recap" {
        let days = args["days"].as_u64().unwrap_or(3).clamp(1, 7) as i64;
        let items = match crate::agent::journal::Journal::recap(&ctx.root, days) {
            Ok(items) => items,
            Err(e) => return Outcome::err(format!("journal recap failed: {e:#}")),
        };
        return Outcome::ok(recap_text(&items, days));
    }
    if op != "read" {
        return Outcome::err("journal op must be 'read' or 'recap'");
    }

    let session_arg = args["session"].as_str().unwrap_or("current");
    let show_session = session_arg == "all";
    let (records, label) = match session_arg {
        "current" | "" => (
            crate::agent::journal::Journal::records_for(&ctx.root, &ctx.session_id),
            ctx.session_id.clone(),
        ),
        "all" => (
            crate::agent::journal::Journal::records(&ctx.root),
            "all sessions".to_string(),
        ),
        other => {
            // a session id is a bare file stem (or 4+ char prefix from recap), never a path
            if other.contains('/') || other.contains('\\') || other.contains("..") {
                return Outcome::err(format!("bad session id '{other}'"));
            }
            let resolved = crate::agent::journal::Journal::resolve_session_id(&ctx.root, other)
                .unwrap_or_else(|| other.to_string());
            (
                crate::agent::journal::Journal::records_for(&ctx.root, &resolved),
                resolved,
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
    // is needed when merging records across all sessions.
    if show_session {
        records.sort_by(|a, b| a.ts.cmp(&b.ts).then(a.seq.cmp(&b.seq)));
    }

    let kind = args["kind"].as_str();
    let query = args["query"].as_str().map(str::to_lowercase);

    let rendered: Vec<String> = records
        .iter()
        .filter(|r| kind.is_none_or(|k| r.kind == k))
        .map(|r| journal_line_with_session(r, show_session))
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
    if kind.is_none() && query.is_none() {
        out.push_str("\nhint: unfiltered list. For \"what happened recently\" use journal op=recap (days=3, session=all).");
    }
    if start > 0 {
        out.push_str(&format!(
            "; {start} older matches — narrow with 'query'/'kind' or raise 'last'"
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

/// Rendered recap: a day header, then one entry per session. Bounded on
/// purpose — a busy week must cost one tool result, not a transcript, which is
/// the mistake the diary made by pasting the whole session in every day.
fn recap_text(items: &[crate::agent::journal::Recap], days: i64) -> String {
    const SESSIONS_PER_DAY: usize = 6;
    const FILES_SHOWN: usize = 6;
    if items.is_empty() {
        return format!("recap: nothing recorded in the last {days} days");
    }
    let mut out = format!(
        "recap · {} sessions in the last {days} days · local dates\n",
        items.len()
    );
    let mut day = String::new();
    let mut shown = 0usize;
    let mut omitted = 0usize;
    for r in items {
        if r.day != day {
            day = r.day.clone();
            shown = 0;
            out.push_str(&format!("— {day} —\n"));
        }
        if shown >= SESSIONS_PER_DAY {
            omitted += 1;
            continue;
        }
        shown += 1;
        let head: String = r.session.chars().take(8).collect();
        out.push_str(&format!("  {head} {}→{}  ", r.from, r.to));
        match &r.ask {
            Some(ask) => out.push_str(&format!("«{ask}»\n")),
            None => out.push_str("(no plan)\n"),
        }
        for plan in &r.plans {
            out.push_str(&format!(
                "    plan {} · {} · {}/{} steps",
                crate::agent::tools::specs::clip(&plan.id, 26),
                plan.status,
                plan.closed,
                plan.total
            ));
            // A second plan in one session means the direction moved; its goal
            // is the information, and repeating the first one is noise.
            if r.ask.as_deref() != Some(plan.goal.as_str()) {
                out.push_str(&format!(" · «{}»", plan.goal));
            }
            out.push('\n');
            if !plan.unfinished.is_empty() {
                out.push_str(&format!(
                    "      unfinished: {}\n",
                    plan.unfinished.join(" · ")
                ));
            }
        }
        if !r.files.is_empty() {
            let listed: Vec<String> = r
                .files
                .iter()
                .take(FILES_SHOWN)
                .map(|(path, added, removed)| format!("{path} +{added}/-{removed}"))
                .collect();
            let more = r.files.len().saturating_sub(FILES_SHOWN);
            let tail = if more > 0 {
                format!(" · +{more} more")
            } else {
                String::new()
            };
            out.push_str(&format!("    files: {}{tail}\n", listed.join(" · ")));
        }
        let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
        out.push_str(&format!(
            "    {} · {} failed · {} notes\n",
            plural(r.calls, "call"),
            r.failures,
            plural(r.notes, "note")
        ));
    }
    if omitted > 0 {
        out.push_str(&format!(
            "  +{omitted} sessions omitted (cap per day) — narrow with days= or session=\n"
        ));
    }
    out
}

/// One rendered journal line: `j#<seq> <time> <kind> [session=S] [step=N] ([agent]) | body`.
/// Everything after `|` is a kind-specific summary with a generic k=v fallback.
fn journal_line_with_session(r: &crate::agent::journal::Record, show_session: bool) -> String {
    let when = chrono::DateTime::parse_from_rfc3339(&r.ts)
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|_| r.ts.clone());
    let mut line = format!("j#{} {when} {}", r.seq, r.kind);
    if show_session && !r.session.is_empty() {
        let short_session: String = r.session.chars().take(8).collect();
        line.push_str(&format!(" session={short_session}"));
    }
    if let Some(step) = &r.step {
        line.push_str(&format!(" step={step}"));
    }
    if r.agent != "main" {
        line.push_str(&format!(" [{}]", r.agent));
    }
    let body = specs::clip(&journal_body(r), 160);
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
            let head = parts.join(" ");
            match s("summary").map(str::trim).filter(|sum| !sum.is_empty()) {
                Some(sum) => {
                    let clean = sum.split_whitespace().collect::<Vec<_>>().join(" ");
                    format!("{head} — {clean}")
                }
                None => head,
            }
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
        "note" => match (s("note"), s("text")) {
            (Some(kind), Some(text)) => format!("{kind}: {text}"),
            _ => s("text").unwrap_or_default().to_string(),
        },
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
        "undo" | "redo" => {
            let mut parts: Vec<String> = Vec::new();
            if let Some(step) = s("step") {
                parts.push(format!("step={step}"));
            }
            if let Some(cp) = s("to_checkpoint")
                && !cp.is_empty()
            {
                parts.push(format!("to={}", specs::clip(cp, 12)));
            }
            if let Some(files) = f.get("files").and_then(Value::as_array) {
                let list: Vec<&str> = files.iter().filter_map(Value::as_str).take(4).collect();
                if !list.is_empty() {
                    parts.push(format!("files=[{}]", list.join(", ")));
                }
            }
            parts.join(" ")
        }
        "memory" => {
            let scope = s("scope").unwrap_or("project");
            let section = s("section").unwrap_or("Project");
            let op = s("op").unwrap_or("written");
            let text = s("text").unwrap_or("");
            if text.is_empty() {
                format!("{scope}/{section} ({op})")
            } else {
                format!("{scope}/{section} ({op}): {text}")
            }
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

/// The `plan` tool: one operation per call, validated by the host.
fn resolve_step_id(args: &mut Value, root: &std::path::Path, session_id: &str) {
    const STEP_OPS: [&str; 2] = ["finish", "cancel"];
    let Some(obj) = args.as_object_mut() else {
        return;
    };
    // Coerce numeric id / step_id / after (`1` -> `"1"`)
    for key in ["id", "step_id", "after"] {
        if let Some(n) = obj.get(key).and_then(Value::as_u64) {
            obj.insert(key.into(), Value::String(n.to_string()));
        } else if let Some(n) = obj.get(key).and_then(Value::as_i64) {
            obj.insert(key.into(), Value::String(n.to_string()));
        }
    }
    if let Some(arr) = obj.get_mut("ids").and_then(Value::as_array_mut) {
        for item in arr.iter_mut() {
            if let Some(n) = item.as_u64() {
                *item = Value::String(n.to_string());
            } else if let Some(n) = item.as_i64() {
                *item = Value::String(n.to_string());
            }
        }
    }
    // Coerce single string `criteria` / `constraints` / `acceptance` into a 1-element array
    for key in ["criteria", "acceptance", "constraints"] {
        if let Some(s) = obj.get(key).and_then(Value::as_str).map(str::to_string) {
            obj.insert(key.into(), Value::Array(vec![Value::String(s)]));
        }
    }
    // Infer `op = "create"` when `op` was omitted and `goal` + `steps` were provided
    if !obj.contains_key("op")
        && obj.get("goal").and_then(Value::as_str).is_some()
        && obj.contains_key("steps")
    {
        obj.insert("op".into(), Value::String("create".into()));
    }
    let Some(op) = obj.get("op").and_then(Value::as_str).map(str::to_string) else {
        return;
    };
    if op == "finish" && !obj.contains_key("summary") {
        for alias in ["message", "text", "note", "result"] {
            if let Some(val) = obj.remove(alias).and_then(|v| v.as_str().map(String::from)) {
                obj.insert("summary".into(), val.into());
                break;
            }
        }
    }
    if matches!(op.as_str(), "cancel" | "abandon" | "block_plan")
        && !obj.contains_key("reason")
    {
        for alias in ["message", "text", "note", "summary"] {
            if let Some(val) = obj.remove(alias).and_then(|v| v.as_str().map(String::from)) {
                obj.insert("reason".into(), val.into());
                break;
            }
        }
    }
    if !STEP_OPS.contains(&op.as_str()) {
        return;
    }
    if !obj.contains_key("id")
        && let Some(step_id) = obj
            .remove("step_id")
            .and_then(|v| v.as_str().map(String::from))
    {
        obj.insert("id".into(), step_id.into());
    }
    let Ok(Some(active)) = plan::open_active_for_session(root, Some(session_id)) else {
        return;
    };
    let given = obj.get("id").and_then(Value::as_str).map(String::from);
    // If the model called `cancel` with the plan's own ULID (or no step id) AND
    // provided a valid 3+ word reason, treat it as `abandon` of the whole plan.
    if op == "cancel" {
        let targets_whole_plan = match &given {
            None => true,
            Some(id) => id.trim() == active.id,
        };
        if targets_whole_plan
            && let Some(reason) = obj.get("reason").and_then(Value::as_str)
            && plan::validate_surrender_reason(reason, "abandoning a plan").is_ok()
        {
            obj.insert("op".into(), Value::String("abandon".into()));
            obj.remove("id");
        }
        return;
    }
    if op != "finish" {
        return;
    }
    let missing_or_plan_id = match &given {
        None => true,
        Some(id) => id.trim().is_empty() || *id == active.id,
    };
    if !missing_or_plan_id {
        return;
    }
    let mut candidates = active
        .steps
        .iter()
        .filter(|s| matches!(s.status, plan::StepStatus::InProgress | plan::StepStatus::Reopened));
    let (Some(first), None) = (candidates.next(), candidates.next()) else {
        return;
    };
    if given.as_deref() == Some(active.id.as_str()) {
        obj.insert("_was_plan_id".into(), Value::Bool(true));
    }
    obj.insert("id".into(), first.id.clone().into());
}

pub(crate) fn plan_op(ctx: &mut ToolCtx, args: &Value) -> Outcome {
    let mut args = args.clone();
    resolve_step_id(&mut args, &ctx.root, &ctx.session_id);
    let was_plan_id = args
        .as_object_mut()
        .and_then(|o| o.remove("_was_plan_id"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let raw_op = args.get("op").and_then(|v| v.as_str()).unwrap_or("");
    if raw_op == "verify" {
        return Outcome::err(
            "verify is retired: run the command yourself (bash) and work from its observed result",
        );
    }
    if raw_op == "show" {
        return Outcome::err(
            "no plan show tool: the current plan with its step ids is in your context above — use those ids, never re-read the plan",
        );
    }
    if raw_op == "start" {
        if let Ok(Some(active)) = plan::open_active_for_session(&ctx.root, Some(&ctx.session_id))
            && let Some(cur) = active
                .steps
                .iter()
                .find(|s| s.status == plan::StepStatus::InProgress)
        {
            return Outcome::ok(format!(
                "step {} (\"{}\") is already in progress — steps start automatically; perform the work and call plan(op=\"finish\", id=\"{}\", summary=\"...\") when done",
                cur.id, cur.title, cur.id
            ));
        }
        return Outcome::err("no active plan: create one with op=create first");
    }
    if raw_op == "complete" {
        if let Ok(Some(active)) = plan::open_active_for_session(&ctx.root, Some(&ctx.session_id)) {
            let open: Vec<String> = active
                .steps
                .iter()
                .filter(|s| !matches!(s.status, plan::StepStatus::Done | plan::StepStatus::Cancelled))
                .map(|s| s.id.clone())
                .collect();
            if !open.is_empty() {
                return Outcome::err(format!(
                    "steps still open ({}) — finish or cancel them; the plan completes automatically when the last step closes",
                    open.join(", ")
                ));
            }
        }
        return Outcome::ok("all steps are closed and the plan is completed");
    }
    let op: plan::Op = match serde_json::from_value(args.clone()) {
        Ok(op) => op,
        Err(e) => {
            let hint = match plan::open_active_for_session(&ctx.root, Some(&ctx.session_id)) {
                Ok(Some(active)) if !active.steps.is_empty() => format!(
                    " — active plan {} steps: {}",
                    active.id,
                    active
                        .steps
                        .iter()
                        .map(|s| format!(
                            "{} [{}] {}",
                            s.id,
                            s.status.as_str(),
                            s.title.chars().take(48).collect::<String>()
                        ))
                        .collect::<Vec<_>>()
                        .join(" · ")
                ),
                _ => " — the current plan with its valid ops is in your context above".to_string(),
            };
            return Outcome::err(format!("plan op rejected: {e}{hint}"));
        }
    };
    if matches!(op, plan::Op::Join { .. }) {
        return Outcome::err(
            "plan op rejected: join is host-only — sessions join a plan via subagent spawn, not this op",
        );
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
                hint: "work that plan instead: finish its steps, do not create another one. The current plan is in your context above".to_string(),
            }),
            Ok(None) => {
                match plan::create(goal, constraints, criteria, steps, &limits) {
                    Ok(mut created) => {
                        created.sessions = vec![ctx.session_id.clone()];
                        let id = created.id.clone();
                        let step_count = created.steps.len();
                        let first_step_id = created.steps.first().map(|s| s.id.clone());
                        let args = serde_json::json!({
                            "goal": created.goal.text,
                            "constraints": created.constraints,
                            "criteria": created.criteria,
                            "steps": created.steps.iter().map(|s| serde_json::json!({
                                "title": s.title,
                            })).collect::<Vec<_>>(),
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
                                if let Some(ref s1) = first_step_id {
                                    ctx.current_step = Some(s1.clone());
                                    if let Ok(Some(sha)) =
                                        crate::agent::checkpoints::snapshot_boundary(
                                            &ctx.root,
                                            ctx.shadow_store,
                                            ctx.checkpoint_chain(),
                                            &format!("step_{s1}_start"),
                                        )
                                    {
                                        ctx.journal.push((sha, format!("step_{s1}_start")));
                                    }
                                }
                                let steps = created
                                    .steps
                                    .iter()
                                    .map(|s| {
                                        format!(
                                            "{} {}",
                                            s.id,
                                            s.title.chars().take(48).collect::<String>()
                                        )
                                    })
                                    .collect::<Vec<_>>()
                                    .join(" · ");
                                Outcome::ok(format!(
                                    "plan {id} created with {step_count} steps (step 1 in progress): {steps}"
                                ))
                            }
                            Err(e) => Outcome::err(format!("plan write failed: {e:#}")),
                        }
                    }
                    Err(r) => rejection(r),
                }
            }
            Err(e) => Outcome::err(format!("plan store unreadable: {e:#}")),
        },
        other => {
            if let plan::Op::Cancel {
                id: Some(ref target_id),
                ..
            } = other
                && plan::list_active(&ctx.root)
                    .iter()
                    .any(|p| p.id == *target_id)
            {
                return Outcome::err(
                    serde_json::json!({
                        "ok": false,
                        "code": "abandon_user_only",
                        "reason": format!("plan {target_id} cannot be cancelled as a step"),
                        "hint": "use op=\"abandon\" with a quoted reason to abandon the whole plan, or ask the user to delete it (/plan delete)",
                    })
                    .to_string(),
                );
            }
            let mut active = match plan::open_active_for_session(&ctx.root, Some(&ctx.session_id)) {
                Ok(Some(p)) => p,
                Ok(None) => {
                    return Outcome::err(
                        "no active plan: create one with op=create first".to_string(),
                    );
                }
                Err(e) => return Outcome::err(format!("plan store unreadable: {e:#}")),
            };
            let prev_in_progress = active
                .steps
                .iter()
                .find(|s| s.status == plan::StepStatus::InProgress)
                .map(|s| s.id.clone());
            // Guard against a model calling `finish(id=<plan_ulid>, summary="Starting ...")`
            // right after `create` before any work was performed on step 1.
            if was_plan_id
                && let plan::Op::Finish {
                    ref id,
                    ref ids,
                    ref summary,
                } = other
                && ids.is_empty()
                && id == "1"
                && active.steps.first().is_some_and(|s| s.evidence.is_empty())
            {
                let sum_low = summary.trim().to_lowercase();
                if sum_low.starts_with("start") || sum_low.starts_with("begin") {
                    let s1_title = active
                        .steps
                        .first()
                        .map(|s| s.title.as_str())
                        .unwrap_or("step 1");
                    return Outcome::ok(format!(
                        "step 1 (\"{s1_title}\") is already in progress — perform the work for step 1 first, then call plan(op=\"finish\", id=\"1\", summary=\"...\") when step 1 is complete"
                    ));
                }
            }
            let finishing = match &other {
                plan::Op::Finish { id, ids, .. } => {
                    if let Some(last) = ids.last() {
                        Some(last.trim().to_string())
                    } else if id.trim().is_empty() {
                        prev_in_progress.clone()
                    } else {
                        Some(id.rsplit(',').next().unwrap_or(id).trim().to_string())
                    }
                }
                _ => None,
            };
            let op_value = serde_json::to_value(&other).unwrap_or(serde_json::Value::Null);
            let op_name = op_value
                .get("op")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown")
                .to_string();
            match plan::apply(&mut active, other, &limits) {
                Ok(applied) => {
                    if let Err(e) = plan::commit(
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
                    let new_in_progress = if active.status == plan::PlanStatus::Active {
                        active
                            .steps
                            .iter()
                            .find(|s| s.status == plan::StepStatus::InProgress)
                            .map(|s| s.id.clone())
                    } else {
                        None
                    };
                    if let Some(ref next_id) = new_in_progress
                        && new_in_progress != prev_in_progress
                        && let Ok(Some(sha)) = crate::agent::checkpoints::snapshot_boundary(
                            &ctx.root,
                            ctx.shadow_store,
                            ctx.checkpoint_chain(),
                            &format!("step_{next_id}_start"),
                        )
                    {
                        ctx.journal.push((sha, format!("step_{next_id}_start")));
                    }
                    ctx.current_step = new_in_progress;
                    match applied {
                        plan::Applied::Updated { message } => {
                            let msg = with_blast_radius(ctx, finishing.as_deref(), &active, message);
                            Outcome::ok(msg)
                        }
                        plan::Applied::Completed { message } => {
                            let mut msg =
                                with_blast_radius(ctx, finishing.as_deref(), &active, message);
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
