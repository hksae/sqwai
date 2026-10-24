# -*- coding: utf-8 -*-
# Patch 2: gates, grants, journal attribution, plan error copy, last_message_at.

# ---------- loop_ask.rs ----------
p = 'src/agent/loop_task/loop_ask.rs'
s = open(p, encoding='utf-8').read()

old = """/// Why a write beyond the project root asks. The workspace jail is a
/// prompt, not a wall: full access lifts it like every other prompt, and
/// host-owned state (`.sqwai/`) is root-relative — it can never appear on
/// this branch.
const OUTSIDE_ROOT_REASON: &str = "writes outside the project directory";"""
new = """/// Why a leap beyond the project root asks. The workspace jail is a
/// prompt, not a wall: full access never asks (every path resolves), and
/// an approval opens the target directory for reads AND writes. Host-owned
/// state (`.sqwai/`) is root-relative — it can never appear on this branch.
pub(crate) const OUTSIDE_WRITE_REASON: &str = "writes outside the project directory";
pub(crate) const OUTSIDE_READ_REASON: &str = "reads outside the project directory";"""
assert s.count(old) == 1, "consts"
s = s.replace(old, new)

old = "async fn approval_dialog("
new = "pub(crate) async fn approval_dialog("
assert s.count(old) == 1
s = s.replace(old, new)

# mutating_tool_gate: new outside branch
old = """    let name = call.name.as_str();
    let granted = tool_grants.iter().any(|t| t == name);
    // the workspace boundary, asked as an approval rather than refused
    if matches!(name, "write" | "edit" | "multi_edit")
        && let Some(path) = tools::call_path(name, &call.args)
        && ctx.write_escapes(&path)
    {
        if mode == ApprovalMode::Full || granted {
            ctx.outside_write_permit = true;
            return None;
        }
        let decision = approval_dialog(
            format!("{name} {path}"),
            OUTSIDE_ROOT_REASON,
            format!("{name} *"),
            tx,
            ctl,
            next_id,
        )
        .await;
        match decision {
            Some(ApprovalDecision::Deny) => {
                return Some(tools::Outcome::err(format!(
                    "{name} denied by user ({OUTSIDE_ROOT_REASON})"
                )))
            }
            Some(ApprovalDecision::AlwaysSession) => tool_grants.push(name.to_string()),
            Some(ApprovalDecision::RunOnce) => {}
            None => return Some(tools::Outcome::err("tui closed awaiting approval")),
        }
        ctx.outside_write_permit = true;
        // this exact call was just confirmed — ask mode has nothing to add
        return None;
    }"""
new = """    let name = call.name.as_str();
    let granted = tool_grants.iter().any(|t| t == name);
    // the workspace boundary, asked as an approval rather than refused:
    // one yes opens the target directory for the run (or the session)
    if matches!(name, "write" | "edit" | "multi_edit")
        && let Some(path) = tools::call_path(name, &call.args)
        && ctx.outside_uncovered(&path)
    {
        // full access and already-granted directories resolve on their own
        // (outside_all / outside_roots); there is nothing to ask
        if mode == ApprovalMode::Full {
            return None;
        }
        let dir = ctx
            .outside_grant_dir(&path)
            .map(|d| d.to_string_lossy().to_string())
            .unwrap_or_default();
        let decision = approval_dialog(
            format!("{name} {path}"),
            OUTSIDE_WRITE_REASON,
            format!("{dir}/**"),
            tx,
            ctl,
            next_id,
        )
        .await;
        match decision {
            Some(ApprovalDecision::Deny) => {
                return Some(tools::Outcome::err(format!(
                    "{name} denied by user ({OUTSIDE_WRITE_REASON})"
                )))
            }
            Some(ApprovalDecision::AlwaysSession) => {
                if let Some(d) = ctx.outside_grant_dir(&path) {
                    ctx.outside_roots.push(d.clone());
                    ctx.outside_granted.push(d);
                }
            }
            Some(ApprovalDecision::RunOnce) => {
                if let Some(d) = ctx.outside_grant_dir(&path) {
                    ctx.outside_roots.push(d);
                }
            }
            None => return Some(tools::Outcome::err("tui closed awaiting approval")),
        }
        // this exact call was just confirmed — ask mode has nothing to add
        return None;
    }"""
assert s.count(old) == 1, "gate outside branch"
s = s.replace(old, new)

# read_call_path helper + read gate reuse: add near mutating_tool_gate end (before run_tool_blocking doc)
old = "/// Execute a tool handler on a dedicated blocking thread."
new = """/// The path a read-only tool points at, when it has one: `read`/`ls`/
/// `outline` via `call_path`, `grep` by its own `path` argument. `glob`
/// patterns are not paths and stay out.
pub(crate) fn read_call_path(name: &str, args: &serde_json::Value) -> Option<String> {
    if name == "grep" {
        return args
            .get("path")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
    }
    tools::call_path(name, args)
}

/// Execute a tool handler on a dedicated blocking thread."""
assert s.count(old) == 1
s = s.replace(old, new)

# bash outside check: switch to the target-returning probe
old = """    if mode != ApprovalMode::Full
        && subagent_depth == 0
        && needs_approval.is_none()
        && tools::bash_writes_outside(ctx, &command)
    {
        needs_approval = Some(OUTSIDE_ROOT_REASON.to_string());
    }"""
new = """    if mode != ApprovalMode::Full
        && subagent_depth == 0
        && needs_approval.is_none()
        && let Some(target) = tools::first_outside_target(ctx, &command)
    {
        // remembered for the AlwaysSession answer below: the directory the
        // offending target lives in
        *outside_target = ctx.outside_grant_dir(&target);
        needs_approval = Some(OUTSIDE_WRITE_REASON.to_string());
    }"""
assert s.count(old) == 1, "bash outside"
s = s.replace(old, new)

# bash_call signature: add outside_target scratch
old = """    subagent_depth: u8,
    approval: ApprovalMode,
    journal: &mut Option<crate::agent::journal::Journal>,
) -> tools::Outcome {
    let command = call.args["command"].as_str().unwrap_or("").to_string();"""
new = """    subagent_depth: u8,
    approval: ApprovalMode,
    journal: &mut Option<crate::agent::journal::Journal>,
    outside_target: &mut Option<std::path::PathBuf>,
) -> tools::Outcome {
    let command = call.args["command"].as_str().unwrap_or("").to_string();"""
assert s.count(old) == 1, "bash sig"
s = s.replace(old, new)

# AlwaysSession in bash_call: also open the directory
old = """            ApprovalDecision::AlwaysSession => {
                if let (Some(kind), Some(writer)) = (taint_kind, journal.as_mut()) {
                    crate::agent::trust::ack_egress(writer, kind);
                }
                always_allow.push(Grant::from_command(&command, reason));
            }"""
new = """            ApprovalDecision::AlwaysSession => {
                if let (Some(kind), Some(writer)) = (taint_kind, journal.as_mut()) {
                    crate::agent::trust::ack_egress(writer, kind);
                }
                always_allow.push(Grant::from_command(&command, reason));
                // an approved outside write opens its directory too, so the
                // follow-up file operations there stop asking
                if let Some(dir) = outside_target.take() {
                    ctx.outside_roots.push(dir.clone());
                    ctx.outside_granted.push(dir);
                }
            }"""
assert s.count(old) == 1, "bash always"
s = s.replace(old, new)
open(p, 'w', encoding='utf-8', newline='').write(s)

# ---------- policy.rs ----------
p = 'src/agent/tools/policy.rs'
s = open(p, encoding='utf-8').read()
old = """/// Does this command write outside the project root? The main agent's
/// workspace boundary, asked as an approval rather than refused: the
/// targets are the same tokens the subagent scope gate sees, null sinks
/// filtered, resolution shared with the write jail.
pub(crate) fn bash_writes_outside(ctx: &ToolCtx, command: &str) -> bool {
    bash_write_targets(command)
        .into_iter()
        .filter(|t| !is_null_sink(t))
        .any(|t| ctx.shell_target_escapes(&t))
}"""
new = """/// The first write target of this command that lands outside the project
/// root AND outside every approved directory — the main agent's workspace
/// boundary, asked as an approval rather than refused. Targets are the
/// same tokens the subagent scope gate sees, null sinks filtered,
/// resolution shared with the write jail.
pub(crate) fn first_outside_target(ctx: &ToolCtx, command: &str) -> Option<String> {
    bash_write_targets(command)
        .into_iter()
        .filter(|t| !is_null_sink(t))
        .find(|t| {
            let bare = t.trim().trim_matches(|c| c == '"' || c == '\\'');
            !bare.is_empty() && ctx.outside_uncovered(bare)
        })
}"""
assert s.count(old) == 1
s = s.replace(old, new)
open(p, 'w', encoding='utf-8', newline='').write(s)

# ---------- tools/mod.rs export ----------
p = 'src/agent/tools/mod.rs'
s = open(p, encoding='utf-8').read()
old = "pub(crate) use policy::{bash_scope_hit, bash_writes_outside, register_subagent_scope, take_subagent_scope};"
new = "pub(crate) use policy::{bash_scope_hit, first_outside_target, register_subagent_scope, take_subagent_scope};"
assert s.count(old) == 1
s = s.replace(old, new)
open(p, 'w', encoding='utf-8', newline='').write(s)
print("loop_ask/policy ok")
