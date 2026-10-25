//! Finish-time decorators and the rejection envelope.
//!
//! The verification machinery that lived here (baselines, freezes, rungs,
//! receipts, the `verify` op) was retired 2026-10-06: the host observes and
//! records, it no longer certifies or blocks. What survives are the advisory
//! lines attached to a successful `finish` — all of them facts from the
//! journal, none of them gates — and the rejection envelope shared by the
//! plan dispatcher.

use super::Outcome;
use super::ctx::ToolCtx;
use crate::plan;
use serde_json::json;

/// One-line blast radius on `finish`: the files this step actually changed,
/// from the journal's plan-visible `file_diff` chain (all sessions — subagent
/// writes count). Shell baselines and zero-change records are not plan
/// evidence. Silent when the step wrote nothing (research steps).
/// Informational, never a refusal: scope made visible.
pub(crate) fn with_blast_radius(
    ctx: &ToolCtx,
    finished_step: Option<&str>,
    active: &plan::Plan,
    message: String,
) -> String {
    let Some(step) = finished_step else {
        return message;
    };
    let paths = crate::agent::journal::Journal::step_changed_paths(
        &ctx.root,
        Some(&active.id),
        step,
    )
    .unwrap_or_default();
    if paths.is_empty() {
        return message;
    }
    const SHOW: usize = 6;
    let list = if paths.len() <= SHOW {
        paths.join(", ")
    } else {
        format!(
            "{}, … (+{} more)",
            paths[..SHOW].join(", "),
            paths.len() - SHOW
        )
    };
    format!(
        "{message}\nblast radius: step {step} touched {} file(s) ({list})",
        paths.len()
    )
}

/// One advisory line on a tree-reading call (`git_status`, `git_diff`): which
/// of the dirty paths this session's journal has no write of. Without it a
/// model that finds a modified file assumes it modified the file — measured in
/// the first live session, where another agent's uncommitted edits set off a
/// check-commit-close ritual over work the model never took on. The journal
/// knows who wrote what, so saying so is showing, not gating: nothing here
/// refuses, and the wording names the ways a path can be dirty without a
/// record (shell writes leave no `file_diff`, humans and sibling sessions
/// write outside this journal).
pub(crate) fn with_authorship(ctx: &ToolCtx, mut outcome: Outcome) -> Outcome {
    if !outcome.ok {
        return outcome;
    }
    let dirty = super::git::dirty_paths(ctx);
    if dirty.is_empty() {
        return outcome;
    }
    let written: Vec<String> =
        crate::agent::journal::Journal::records_for(&ctx.root, &ctx.session_id)
            .unwrap_or_default()
            .iter()
            .filter(|record| record.kind == "file_diff")
            .filter_map(|record| record.fields.get("path").and_then(|v| v.as_str()))
            .map(str::to_string)
            .collect();
    let foreign: Vec<String> = dirty
        .into_iter()
        .filter(|path| !written.iter().any(|w| w == path))
        // name only what this result actually shows, so a scoped diff of the
        // model's own file is not interrupted by unrelated dirt elsewhere
        .filter(|path| outcome.output.contains(path.as_str()))
        .collect();
    if foreign.is_empty() {
        return outcome;
    }
    const SHOW: usize = 6;
    let list = if foreign.len() <= SHOW {
        foreign.join(", ")
    } else {
        format!(
            "{}, … (+{} more)",
            foreign[..SHOW].join(", "),
            foreign.len() - SHOW
        )
    };
    outcome.output.push_str(&format!(
        "\nauthorship: this session's journal records no write of {list} — they predate it, \
         came from a human or another session, or were written through bash (which records no \
         diff). Not yours to commit unless you know where they came from."
    ));
    outcome
}

/// Rejections are a normal tool result the model can act on.
pub(crate) fn rejection(r: plan::Rejection) -> Outcome {
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
