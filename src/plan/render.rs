use super::{
    AcceptanceKind, AcceptanceStatus, Applied, GoalRevision, Plan, PlanStatus,
    Receipt, Rejection, StepStatus, ValidationStatus, accept, commit, digest_paths,
    differential_current, ladder_rung, now, open_active_for_session, proven_failing,
    reject, signatures_current, snapshot_current, state_digest, waived_constraint_indices,
};
use anyhow::Result;
use std::path::Path;


// ---------------------------------------------------------------- host-only

/// Apply a goal revision the user accepted (§2.1.6). Host-only.
pub fn set_goal(plan: &mut Plan, text: String, source: &str, reason: Option<String>) {
    let previous = std::mem::replace(&mut plan.goal.text, text.clone());
    plan.goal.history.push(GoalRevision {
        text: previous,
        source: plan.goal.source.clone(),
        at: now(),
        reason,
    });
    plan.goal.source = source.to_string();
    for step in &mut plan.steps {
        if step.status == StepStatus::Pending {
            step.stale_goal = Some(true);
        }
    }
    plan.revision += 1;
}

/// User waives an acceptance item (§2.1.7). Host-only.
pub fn waive(plan: &mut Plan, index: usize, reason: &str) -> Result<(), Rejection> {    if index >= plan.acceptance.len() {
        return Err(Rejection::new(
            "unknown_acceptance",
            format!("no acceptance item {index}"),
            "call /plan to see the acceptance list",
        ));
    }
    let item = &mut plan.acceptance[index];
    item.status = AcceptanceStatus::Waived;
    // waived is a validation state too: the complete gate reads validation,
    // and waived items are never auto-invalidated.
    item.validation.status = ValidationStatus::Waived;
    item.by = Some("user".to_string());
    item.reason = Some(reason.to_string());
    plan.revision += 1;
    Ok(())
}

/// User confirms a manual acceptance after inspecting the result (§2.1.4).
/// Host-only (TUI `/plan confirm`). Only `manual:` items qualify — commands
/// and evidence-backed text have their own verify paths. Records a
/// `manual_confirmation` journal record and a point-in-time receipt over
/// the same digest inputs cmd checks use, so later file moves still stale
/// the item instead of silently outliving its confirmation.
pub fn confirm(
    root: &Path,
    session_id: &str,
    plan: &mut Plan,
    index: usize,
    reason: &str,
) -> Result<Applied, Rejection> {
    if index >= plan.acceptance.len() {
        return reject(
            plan,
            "unknown_acceptance",
            format!("no acceptance item {index}"),
            "call /plan to see the acceptance list",
        );
    }
    match plan.acceptance[index].kind() {
        AcceptanceKind::Manual(_) => {}
        AcceptanceKind::Command(cmd) => {
            return reject(
                plan,
                "not_manual",
                format!("acceptance {index} runs a command; verify it instead"),
                format!("run the check ({cmd}), or waive the item with /plan waive"),
            );
        }
        AcceptanceKind::Snapshot(cmd) => {
            return reject(
                plan,
                "not_manual",
                format!("acceptance {index} freezes a command's output; verify it instead"),
                format!("run the check ({cmd}), or waive the item with /plan waive"),
            );
        }
        AcceptanceKind::Differential(cmd) => {
            return reject(
                plan,
                "not_manual",
                format!("acceptance {index} compares a command's output old-vs-new; verify it instead"),
                format!("run the check ({cmd}), or waive the item with /plan waive"),
            );
        }
        AcceptanceKind::Signatures(paths) => {
            return reject(
                plan,
                "not_manual",
                format!(
                    "acceptance {index} freezes declaration shapes; verify it instead"
                ),
                format!(
                    "re-read the shapes ({}), or waive the item with /plan waive",
                    paths.join(", ")
                ),
            );
        }
        AcceptanceKind::Text(_) => {
            return reject(
                plan,
                "not_manual",
                format!("acceptance {index} settles on verify-step evidence"),
                "close a verify step with evidence for it, or waive it with /plan waive",
            );
        }
    }
    let paths = digest_paths(plan);
    let digest = state_digest(root, &paths, "");
    let at = now();
    let mut journal = crate::agent::journal::Journal::open(root, session_id)
        .map_err(|e| Rejection::new("journal_unwritable", format!("{e:#}"), ""))?;
    let seq = journal
        .append(
            "manual_confirmation",
            serde_json::json!({
                "acceptance_id": index,
                "by": "user",
                "state_digest": digest,
                "reason": reason,
            }),
        )
        .map_err(|e| Rejection::new("journal_unwritable", format!("{e:#}"), ""))?;
    attach_confirmation(
        plan,
        index,
        reason,
        Receipt {
            session: session_id.to_string(),
            seq,
            state_digest: digest.clone(),
            command: None,
            exit: None,
            at,
            check_definition_hash: None,
            runner: Some("manual".to_string()),
            args: None,
            cwd: Some(root.display().to_string()),
            started_at: None,
            finished_at: None,
            state_before: Some(digest.clone()),
            state_after: Some(digest),
            output_hash: None,
            paths,
        },
    )
}

/// Pure half of `confirm` (and its replay): pin a recorded confirmation to
/// the item. Replay trusts the journaled payload — it must not recompute
/// digests or append records.
pub fn attach_confirmation(
    plan: &mut Plan,
    index: usize,
    reason: &str,
    receipt: Receipt,
) -> Result<Applied, Rejection> {
    if index >= plan.acceptance.len() {
        return reject(
            plan,
            "unknown_acceptance",
            format!("no acceptance item {index}"),
            "call /plan to see the acceptance list",
        );
    }
    let item = &mut plan.acceptance[index];
    item.status = AcceptanceStatus::Passed;
    item.validation.status = ValidationStatus::Passed;
    item.validation.receipts.push(receipt);
    item.by = Some("user".to_string());
    item.reason = Some(reason.to_string());
    accept(plan, format!("acceptance {index} confirmed by user"))
}

/// Host verdict that repeated runs of this check disagreed on the same
/// state (§12.12 three states): the item is flaky, not verified and not
/// plain failed. Idempotent so replay converges; only already-verified
/// (`Passed` status) items can get here — a first red run is
/// `acceptance_failed`, not a disagreement. Waiver is the way out: no
/// green run retries an `Unknown` item back into `passed`.
pub fn apply_flaky(plan: &mut Plan, index: usize) -> bool {
    let Some(item) = plan.acceptance.get_mut(index) else {
        return false;
    };
    if item.status != AcceptanceStatus::Passed
        || item.validation.status == ValidationStatus::Unknown
    {
        return false;
    }
    item.validation.status = ValidationStatus::Unknown;
    plan.revision += 1;
    plan.rejections_in_a_row = 0;
    true
}

/// Mark passed validations stale whose receipt paths intersect `paths`.
/// Returns true when anything changed. Waived items are never touched.
pub fn apply_invalidate(plan: &mut Plan, paths: &[String]) -> bool {
    if paths.is_empty() {
        return false;
    }
    let mut changed = false;
    for item in &mut plan.acceptance {
        if item.validation.status != ValidationStatus::Passed {
            continue;
        }
        let hit = item
            .validation
            .receipts
            .iter()
            .any(|r| r.paths.iter().any(|p| paths.iter().any(|q| q == p)));
        if hit {
            item.validation.status = ValidationStatus::Stale;
            changed = true;
        }
    }
    if changed {
        plan.revision += 1;
        plan.rejections_in_a_row = 0;
    }
    changed
}

/// Host hook after file_diff outcomes: load the session's active plan and
/// journal-first commit an invalidation, but only when something actually
/// went stale — mutating tools must not spam the journal on every call.
pub fn invalidate_on_diff(root: &Path, session_id: &str, paths: &[String]) -> Result<bool> {
    let Some(mut plan) = open_active_for_session(root, Some(session_id))? else {
        return Ok(false);
    };
    if !apply_invalidate(&mut plan, paths) {
        return Ok(false);
    }
    commit(
        root,
        session_id,
        &mut plan,
        "invalidate",
        "host",
        true,
        serde_json::json!({"paths": paths}),
    )?;
    Ok(true)
}

/// Diff helper for stale markers (I4): given the set of (plan_id, index)
/// already announced in chat, return newly-stale acceptance indices plus
/// the pruned set. Pure: the TUI calls it at turn end and pushes one
/// durable status row per new index. Re-verified items drop out, so a
/// second staleness announces again; entries of replaced plans are pruned.
pub fn stale_announcements(
    announced: &std::collections::HashSet<(String, usize)>,
    plan: &Plan,
) -> (Vec<usize>, std::collections::HashSet<(String, usize)>) {
    let mut next: std::collections::HashSet<(String, usize)> = announced
        .iter()
        .filter(|(id, _)| id == &plan.id)
        .cloned()
        .collect();
    let mut fresh = Vec::new();
    for (i, item) in plan.acceptance.iter().enumerate() {
        let key = (plan.id.clone(), i);
        if item.validation.status == ValidationStatus::Stale {
            if next.insert(key) {
                fresh.push(i);
            }
        } else {
            next.remove(&key);
        }
    }
    (fresh, next)
}

// ---------------------------------------------------------------- rendering

/// The plan document shown by `/plan` (§2.1.7).
/// The immutable half of the plan: id, goal, constraints. Changes only when
/// the user (or an accepted proposal) rewrites the goal or the constraint
/// set, so it belongs to the cacheable wire prefix. Waive markers travel
/// here: a waiver re-keys the prefix, which is correct — the model must see
/// the waived constraint, not a stale cached one.
pub fn render_goal(plan: &Plan) -> String {
    let mut out = String::new();
    out.push_str(&format!("plan {}\n", plan.id));
    out.push_str(&format!("goal: {}\n", plan.goal.text));
    if !plan.constraints.is_empty() {
        let waived = waived_constraint_indices(plan);
        let rendered: Vec<String> = plan
            .constraints
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if waived.contains(&i) {
                    format!("{c} [waived]")
                } else {
                    c.clone()
                }
            })
            .collect();
        out.push_str(&format!("constraints: {}\n", rendered.join(" · ")));
    }
    if !plan.checklist.is_empty() {
        out.push_str(&format!("checklist (non-blocking): {}\n", plan.checklist.join(" · ")));
    }
    out
}

/// The moving half of the plan: status, acceptance validation, steps and
/// their summaries. Rebuilt every turn, so it rides the volatile tail —
/// step transitions must never invalidate the cached prefix.
pub fn render_status(plan: &Plan) -> String {
    let c = plan.counts();
    let mut out = String::new();
    out.push_str(&format!("status: {}\n", status_word(plan.status)));
    if let Some(reason) = &plan.blocked_reason {
        out.push_str(&format!("blocked: {reason}\n"));
    }
    if !plan.acceptance.is_empty() {
        out.push_str("acceptance:\n");
        for (i, a) in plan.acceptance.iter().enumerate() {
            out.push_str(&format!("  [{}] {} {}", i, a.status.as_str(), a.text));
            // validation is the complete gate (§2.1.4): surface non-pending
            // states so a stale check is visible before `complete` rejects it
            match a.validation.status {
                ValidationStatus::Pending => {}
                // the third ULTRA state (§12.12): runs disagreed, so the
                // item reports as flaky rather than as failed-or-verified
                ValidationStatus::Unknown => out.push_str(" [flaky — runs disagree]"),
                other => out.push_str(&format!(" [validation: {}]", other.as_str())),
            }
            // ladder walk (§12.12): which rung this item engages
            if let Some(rung) = ladder_rung(a) {
                out.push_str(&format!(" [rung {} {}]", rung.number(), rung.name()));
            }
            // §12.12: a cmd: item with no baseline cannot be settled by a
            // green run, so say so here rather than at the verify that fails
            if matches!(a.kind(), AcceptanceKind::Command(_)) && !proven_failing(a) {
                out.push_str(" [no baseline]");
            }
            // rung 4: a snapshot: item with nothing frozen has no behavior
            // to compare against, so say so here as well
            if matches!(a.kind(), AcceptanceKind::Snapshot(_)) && !snapshot_current(a) {
                out.push_str(" [no snapshot]");
            }
            // rung 3 rides the same frozen record with the inverted verdict
            if matches!(a.kind(), AcceptanceKind::Differential(_)) && !differential_current(a) {
                out.push_str(" [no differential]");
            }
            // rung 5: a signatures: item with nothing frozen has no shapes
            // to compare against
            if matches!(a.kind(), AcceptanceKind::Signatures(_)) && !signatures_current(a) {
                out.push_str(" [no signatures]");
            }
            if !a.inputs.is_empty() {
                out.push_str(&format!(" [inputs: {}]", a.inputs.len()));
            }
            out.push('\n');
        }
    }
    out.push_str(&format!(
        "steps: {} done · {} in progress · {} blocked · {} pending · {} cancelled\n",
        c.done, c.in_progress, c.blocked, c.pending, c.cancelled
    ));
    for f in &plan.folded {
        out.push_str(&format!("  {}\n", f.text));
    }
    for s in &plan.steps {
        let marker = match s.status {
            StepStatus::Done => "[x]",
            StepStatus::InProgress => "[>]",
            StepStatus::Blocked => "[!]",
            StepStatus::Cancelled => "[-]",
            StepStatus::Pending | StepStatus::Reopened => "[ ]",
        };
        let mut line = format!("  {} {} {}", marker, s.id, s.title);
        if s.stale_goal == Some(true) {
            line.push_str("  [stale goal]");
        }
        out.push_str(&line);
        out.push('\n');
        if let Some(reason) = &s.reason {
            out.push_str(&format!("      reason: {reason}\n"));
        }
        if let Some(summary) = &s.summary {
            out.push_str(&format!("      {summary}\n"));
        }
    }
    out
}

pub fn render(plan: &Plan) -> String {
    format!("{}{}", render_goal(plan), render_status(plan))
}

fn status_word(status: PlanStatus) -> &'static str {
    match status {
        PlanStatus::Active => "active",
        PlanStatus::Completed => "completed",
        PlanStatus::Abandoned => "abandoned",
        PlanStatus::Blocked => "blocked",
    }
}

