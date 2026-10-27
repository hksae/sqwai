use super::ctx::ToolCtx;
use super::exec;
use super::fs;
use super::git;
use super::outline;
use super::policy::{bash_scope_hit, in_write_scope, mutation_target_paths, step_epoch_current};
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
    /// host-derived metadata for the event log
    pub file_diff: Option<FileDiff>,
    /// host-derived metadata for all modified files (e.g. multi-file patch)
    pub file_diffs: Vec<FileDiff>,
    /// images the tool returned to the model (view_image today)
    pub images: Vec<crate::providers::ImageAttachment>,
    /// PDF documents the tool returned to the model (view_document today)
    pub documents: Vec<crate::providers::DocumentAttachment>,
    /// the user pressed Esc while this tool was running. Distinct from
    /// an ordinary failure — the event log records `code: "cancelled"` rather
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
            images: Vec::new(),
            documents: Vec::new(),
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
            images: Vec::new(),
            documents: Vec::new(),
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
            images: Vec::new(),
            documents: Vec::new(),
            cancelled: true,
        }
    }
    /// Attach pixels to a successful result. Kept separate from `ok` so every
    /// existing constructor stays image-free by default.
    pub fn with_images(mut self, images: Vec<crate::providers::ImageAttachment>) -> Self {
        self.images = images;
        self
    }

    pub fn with_documents(mut self, documents: Vec<crate::providers::DocumentAttachment>) -> Self {
        self.documents = documents;
        self
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

    /// True when the call actually changed file bytes. A bash checkpoint or
    /// a zero-change baseline is deliberately not a mutation here: plan
    /// suggestions belong after real work, not after recon.
    pub fn changed_any_file(&self) -> bool {
        self.file_diffs
            .iter()
            .chain(self.file_diff.iter())
            .any(|diff| diff.added > 0 || diff.removed > 0)
    }
}

/// (id, owning session, command) of background jobs whose processes are
/// still alive. The undo preflight refuses a restore while any
/// of these run — the writer lock stops in-process dispatch, but an
/// already-running shell would keep writing mid-restore.
pub(crate) fn bg_running_commands_for(session: &str) -> Vec<(u64, String, String)> {
    exec::running_commands_for(session)
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
        && matches!(name, "write" | "edit" | "bash")
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
        && matches!(name, "write" | "edit" | "bash")
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
        if matches!(name, "write" | "edit") {
            let outside = mutation_target_paths(ctx, args)
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
        "view_image" => super::view_image::view(ctx, args),
        "view_document" => super::view_document::view(ctx, args),
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
        "outline" => outline::outline(ctx, args),
        "webfetch" | "websearch" => Outcome::err("web tools must run through the async dispatcher"),
        "bash" => exec::bash_with_workdir(
            ctx,
            args["command"].as_str().unwrap_or_default(),
            args["timeout"].as_u64(),
            args["background"].as_bool().unwrap_or(false),
            args["workdir"].as_str().or_else(|| args["cwd"].as_str()),
        ),
        "bash_output" => exec::bash_output(ctx, args),
        "bash_kill" => exec::bash_kill(ctx, args),
        "sleep" => exec::sleep(ctx, args),
        "plan" => plan_op(ctx, args),
        // direct dispatch never answers "unknown tool"
        "ask_user" => Outcome::err("ask_user is served by the agent loop, not by the dispatcher"),
        "recall_result" => {
            Outcome::err("recall_result is served by the agent loop, not by the dispatcher")
        }
        "skill" => skill_op(ctx, args),
        other => Outcome::err(format!("unknown tool '{other}'")),
    }
}

fn skill_op(ctx: &mut ToolCtx, args: &Value) -> Outcome {
    let name = args["name"].as_str().unwrap_or("").trim();
    if name.is_empty() {
        return Outcome::err("skill 'name' is required");
    }
    let all = crate::prompts::skills::discover(&ctx.skills_cfg, &ctx.root);
    if name.eq_ignore_ascii_case("list") {
        if all.is_empty() {
            return Outcome::ok("no skills available in configured or project skill directories");
        }
        let lines: Vec<String> = all
            .iter()
            .map(|s| {
                let desc = s.description.as_deref().unwrap_or("no description");
                format!("- {} ({}) — {}", s.name, s.scope.as_str(), desc)
            })
            .collect();
        return Outcome::ok(format!(
            "available skills ({}):\n{}",
            all.len(),
            lines.join("\n")
        ));
    }

    let skill = all
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case(name))
        .cloned()
        .or_else(|| {
            let mut m = crate::prompts::skills::filter_by_query(&all, name);
            (m.len() == 1).then(|| m.remove(0))
        });

    let Some(skill) = skill else {
        if all.is_empty() {
            return Outcome::err(format!("skill '{name}' not found (no skills available)"));
        }
        let names: Vec<&str> = all.iter().map(|s| s.name.as_str()).collect();
        return Outcome::err(format!(
            "skill '{name}' not found (available: {})",
            names.join(", ")
        ));
    };

    if let Some(file) = args["file"]
        .as_str()
        .map(str::trim)
        .filter(|f| !f.is_empty())
    {
        return match crate::prompts::skills::read_skill_file(&skill, file) {
            Ok(content) => Outcome::ok(format!(
                "<skill_file skill=\"{}\" file=\"{}\">\n{}\n</skill_file>",
                skill.name, file, content
            )),
            Err(err) => Outcome::err(err),
        };
    }

    let key = skill.name.to_ascii_lowercase();
    if !ctx.activated_skills.insert(key) {
        let files_note = if skill.files.is_empty() {
            String::new()
        } else {
            format!(
                " (bundled files available via `file`: {})",
                skill.files.join(", ")
            )
        };
        return Outcome::ok(format!(
            "skill '{}' is already loaded in your active context above{files_note}",
            skill.name
        ));
    }

    Outcome::ok(crate::prompts::skills::render_skill_detail(&skill))
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
    if matches!(op.as_str(), "cancel" | "abandon" | "block_plan") && !obj.contains_key("reason") {
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
    let mut candidates = active.steps.iter().filter(|s| {
        matches!(
            s.status,
            plan::StepStatus::InProgress | plan::StepStatus::Reopened
        )
    });
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
                Ok(None) => " — there is no active plan; start one with plan(op=\"create\", goal=\"...\", steps=[\"...\", \"...\"])".to_string(),
                _ => " — the current plan with its valid ops is in your context above".to_string(),
            };
            // schema rejections teach the fix in place: a small model that
            // once sent `finish` without `summary` abandoned the plan for
            // the rest of the session instead of resending the one field
            let err = e.to_string();
            let example = if !err.contains("missing field") {
                ""
            } else {
                match raw_op {
                    "finish" => {
                        " — resend as plan(op=\"finish\", id=\"<step>\", summary=\"one sentence: what this step settled\")"
                    }
                    "add" => " — resend as plan(op=\"add\", title=\"...\")",
                    "cancel" => " — resend as plan(op=\"cancel\", id=\"<step>\", reason=\"...\")",
                    "abandon" => " — resend as plan(op=\"abandon\", reason=\"...\")",
                    _ => {
                        " — resend as plan(op=\"create\", goal=\"one sentence\", steps=[\"...\", \"...\"])"
                    }
                }
            };
            return Outcome::err(format!("plan op rejected: {e}{hint}{example}"));
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
                            &ctx.events_session,
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
                                        ctx.checkpoints.push((sha, format!("step_{s1}_start")));
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
                        &ctx.events_session,
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
                        ctx.checkpoints.push((sha, format!("step_{id}_finish")));
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
                        ctx.checkpoints.push((sha, format!("step_{next_id}_start")));
                    }
                    ctx.current_step = new_in_progress;
                    match applied {
                        plan::Applied::Updated { message } => {
                            let msg = with_blast_radius(ctx, finishing.as_deref(), &active, message);
                            Outcome::ok(msg)
                        }
                        plan::Applied::Completed { message } => Outcome::ok(with_blast_radius(
                            ctx,
                            finishing.as_deref(),
                            &active,
                            message,
                        )),
                    }
                }
                Err(r) => {
                    let _ = plan::commit(
                        &ctx.root,
                        &ctx.events_session,
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
