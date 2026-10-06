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
