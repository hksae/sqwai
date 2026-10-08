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

/// Append the open-assumption warning to a successful `finish` (§2.1.4).
///
/// Non-blocking on purpose: the step is already finished when this runs. The
/// point is that an assumption cannot quietly outlive the step that made it —
/// the model either resolves it with `note { resolves }` or carries it
/// forward knowingly.
pub(crate) fn with_assumption_warning(
    ctx: &ToolCtx,
    finished_step: Option<&str>,
    message: String,
) -> String {
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

/// Warn when evidence attached to the finished step predates its last
/// mutation — the record stands, its freshness is what's in question.
pub(crate) fn with_evidence_ts_warning(
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

/// One-line blast radius on `finish`: the files this step wrote, from the
/// journal's `file_diff` chain (all sessions — subagent writes count).
/// Silent when the step wrote nothing (research steps). Informational,
/// never a refusal: scope made visible.
pub(crate) fn with_blast_radius(
    ctx: &ToolCtx,
    finished_step: Option<&str>,
    active: &plan::Plan,
    message: String,
) -> String {
    let Some(step) = finished_step else {
        return message;
    };
    let revert =
        crate::agent::journal::Journal::step_pre_images_in(&ctx.root, Some(&active.id), step)
            .unwrap_or_default();
    let mut paths: Vec<String> = revert
        .files
        .iter()
        .map(|file| file.path.clone())
        .chain(revert.written_since.iter().cloned())
        .collect();
    paths.sort();
    paths.dedup();
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

/// Rejections are a normal tool result the model can act on (§2.1.4).
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
