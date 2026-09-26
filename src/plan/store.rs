use super::{
    Baseline, CheckInput, EvidenceRef, Limits, NewStep, Op, Plan, PlanDraftArgs,
    PlanStatus, Receipt, Rejection, ShapeFreeze, Snapshot, add_acceptance, apply,
    apply_flaky, apply_invalidate, attach_confirmation, create, plans_dir,
    reopen_for_undo, set_baselines, set_goal, set_inputs, set_shapes, set_snapshots,
    verify_acceptance, waive, waive_constraint,
};
use anyhow::{Context, Result};
use std::path::Path;


/// Immutable spawn context a subagent inherits (§2.2.4): which plan step it
/// works on and at which epoch. Mutations and evidence from an older epoch
/// are refused / filtered after the step is reopened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepContext {
    pub plan_id: String,
    pub step_id: String,
    pub step_epoch: u64,
}

// ---------------------------------------------------------------- journal-first commits and replay (§2.1.4, §2.2.1)
//
// Every mutation of a plan file is preceded by a journal `plan` intent
// record carrying the full op args. The record's scoped `session:seq`
// becomes the file's `applied_event` in the same store, so a crash between
// the two is detectable: on load the host replays journal events after the
// cursor. Replay is pure re-application — no commands run, no approvals,
// no evidence gates — which is sound because every replayed op was already
// accepted (and validated, where validation applies) when first journaled.

fn scoped(session: &str, seq: u64) -> String {
    format!("{session}:{seq}")
}

fn split_applied(applied: &Option<String>) -> Option<(String, u64)> {
    let cursor = applied.as_ref()?;
    let (session, seq) = cursor.split_once(':')?;
    Some((session.to_string(), seq.parse().ok()?))
}

/// Journal-first commit: append the intent record, point the file at it,
/// then store. A crash before the append leaves nothing behind (the op only
/// ran in memory); a crash after it heals by `replay`.
pub fn commit(
    root: &Path,
    session_id: &str,
    plan: &mut Plan,
    op: &str,
    by: &str,
    ok: bool,
    args: serde_json::Value,
) -> Result<u64> {
    let mut fields = args.as_object().cloned().unwrap_or_default();
    fields.insert("op".to_string(), serde_json::Value::String(op.to_string()));
    fields.insert(
        "plan_id".to_string(),
        serde_json::Value::String(plan.id.clone()),
    );
    fields.insert("by".to_string(), serde_json::Value::String(by.to_string()));
    fields.insert("ok".to_string(), serde_json::Value::Bool(ok));
    let mut journal = crate::agent::journal::Journal::open(root, session_id)?;
    let seq = journal.append("plan", serde_json::Value::Object(fields))?;
    plan.applied_event = Some(scoped(session_id, seq));
    plan.applied_events.insert(session_id.to_string(), seq);
    store(root, plan)?;
    Ok(seq)
}

#[derive(Debug, Default)]
pub struct ReplayReport {
    pub ops_applied: usize,
    pub evidence_reattached: usize,
    pub plans_healed: Vec<String>,
    pub orphans_rebuilt: Vec<String>,
    pub stalled: Vec<String>,
}

/// Re-apply journaled plan ops the plan files have not caught up with, and
/// re-attach evidence refs whose file update was lost. Files without a
/// cursor (`applied_event`, i.e. written before journal-first) are skipped.
/// Idempotent: a clean tree changes nothing and stores nothing.
pub fn replay(root: &Path) -> Result<ReplayReport> {
    let mut report = ReplayReport::default();
    for plan in list(root) {
        // cursor set: one entry per session that ever committed here, else
        // the legacy single cursor. BTreeMap order is deterministic; each
        // stream heals independently (a stall holds its own stream while
        // the others keep healing).
        let mut cursors: std::collections::BTreeMap<String, u64> = plan.applied_events.clone();
        if cursors.is_empty() {
            if let Some((sess, cursor)) = split_applied(&plan.applied_event) {
                cursors.insert(sess, cursor);
            } else {
                continue;
            }
        }
        let mut plan = plan;
        let mut dirty = false;
        for (sess, cursor) in cursors.iter() {
            let mut records = match crate::agent::journal::Journal::records_for(root, sess) {
                Ok(records) => records,
                Err(_) => continue,
            };
            records.sort_by_key(|record| record.seq);
            // The cursor gates plan ops only. Evidence moves no cursor and
            // is idempotent (deduped by session+seq below), so it re-attaches
            // from anywhere in the stream — even journaled before a crash and
            // overtaken by a later finish. Gating it on the cursor lost the
            // receipt forever whenever the file missed the live attach.
            for record in records.iter() {
                if record.kind == "plan"
                    && record
                        .fields
                        .get("plan_id")
                        .and_then(|value| value.as_str())
                        == Some(plan.id.as_str())
                {
                    if record.seq <= *cursor {
                        continue;
                    }
                    match apply_record(&mut plan, record) {
                        Ok(true) => {
                            plan.applied_event = Some(scoped(sess, record.seq));
                            plan.applied_events.insert(sess.clone(), record.seq);
                            store(root, &plan)?;
                            report.ops_applied += 1;
                            if !report.plans_healed.contains(&plan.id) {
                                report.plans_healed.push(plan.id.clone());
                            }
                        }
                        Ok(false) => {}
                        Err(_) => {
                            // State diverged from what the op was accepted against;
                            // hold this stream's cursor and leave the rest for
                            // a human. Other sessions keep healing below.
                            if !report.stalled.contains(&plan.id) {
                                report.stalled.push(plan.id.clone());
                            }
                            break;
                        }
                    }
                    continue;
                }
                if matches!(
                    record.kind.as_str(),
                    "tool_result" | "file_diff" | "diagnostics"
                ) && record.plan.as_deref() == Some(plan.id.as_str())
                {
                    let Some(step) = record
                        .step
                        .clone()
                        .and_then(|id| plan.steps.iter_mut().find(|step| step.id == id))
                    else {
                        continue;
                    };
                    if !step
                        .evidence
                        .iter()
                        .any(|reference| reference.session == *sess && reference.seq == record.seq)
                    {
                        step.evidence.push(EvidenceRef {
                            session: sess.clone(),
                            seq: record.seq,
                        });
                        report.evidence_reattached += 1;
                        dirty = true;
                    }
                }
            }
        }
        if dirty {
            plan.revision = plan.revision.saturating_add(1);
            store(root, &plan)?;
            if !report.plans_healed.contains(&plan.id) {
                report.plans_healed.push(plan.id.clone());
            }
        }
    }
    report.orphans_rebuilt = replay_orphans(root, &mut report.ops_applied);
    if report.ops_applied > 0 || !report.stalled.is_empty() {
        // Audit trail for the healing itself. `op: replay` is not a plan op
        // and is always skipped on later replays; it moves no cursor.
        for plan_id in report.plans_healed.clone() {
            let sess = applied_session(root, &plan_id);
            if let Some(sess) = sess
                && let Ok(mut journal) = crate::agent::journal::Journal::open(root, &sess)
            {
                let _ = journal.append(
                    "plan",
                    serde_json::json!({
                        "op": "replay",
                        "plan_id": plan_id,
                        "by": "host",
                        "ops_applied": report.ops_applied,
                        "stalled": report.stalled,
                    }),
                );
            }
        }
    }
    Ok(report)
}

fn applied_session(root: &Path, plan_id: &str) -> Option<String> {
    let plan = read_plan_file(root, plan_id)?;
    let (sess, _) = split_applied(&plan.applied_event)?;
    Some(sess)
}

/// Rebuild plans whose create/accept intent was journaled but whose file is
/// missing — the crash landed between the record and the store. A later
/// `plan_deleted` for the same id means the absence is deliberate.
fn replay_orphans(root: &Path, ops_applied: &mut usize) -> Vec<String> {
    let mut rebuilt = Vec::new();
    let dir = root.join(".sqwai").join("journal");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return rebuilt;
    };
    for entry in entries.flatten() {
        if entry.path().extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(sess) = entry
            .path()
            .file_stem()
            .and_then(|name| name.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let records = match crate::agent::journal::Journal::records_for(root, &sess) {
            Ok(records) => records,
            Err(_) => continue,
        };
        for record in records.iter().filter(|record| record.kind == "plan") {
            let fields = &record.fields;
            if fields.get("ok") == Some(&serde_json::Value::Bool(false)) {
                continue;
            }
            let deleted_after = records.iter().any(|other| {
                other.kind == "plan_deleted"
                    && other.seq > record.seq
                    && other.fields.get("plan_id").and_then(|value| value.as_str())
                        == fields.get("result_id").and_then(|value| value.as_str())
            });
            if deleted_after {
                continue;
            }
            match fields.get("op").and_then(|value| value.as_str()) {
                Some("create") => {
                    let Some(result_id) = fields.get("result_id").and_then(|value| value.as_str())
                    else {
                        continue;
                    };
                    if plans_dir(root).join(format!("{result_id}.json")).exists() {
                        continue;
                    }
                    if let Some(plan) = rebuild_created(root, &sess, record.seq, fields) {
                        rebuilt.push(plan.id.clone());
                        *ops_applied += 1;
                    }
                }
                Some("accept_proposal") => {
                    let Some(new_id) = fields.get("new_id").and_then(|value| value.as_str()) else {
                        continue;
                    };
                    if plans_dir(root).join(format!("{new_id}.json")).exists() {
                        continue;
                    }
                    if let Some(plan) = rebuild_accepted(root, &sess, record.seq, fields) {
                        rebuilt.push(plan.id.clone());
                        *ops_applied += 1;
                    }
                }
                _ => {}
            }
        }
    }
    rebuilt
}

/// Frozen proof riders on a create intent: baselines (§12.12), rung-4
/// snapshots, check inputs, rung-5 shapes, and the non-blocking checklist.
/// Restored, never re-run or re-frozen. Shared by the orphan path and the
/// corrupt-file path so both rebuilds heal identically — a rebuild that
/// drops them silently turns proven checks back into smoke tests.
fn restore_create_riders(plan: &mut Plan, fields: &serde_json::Map<String, serde_json::Value>) {
    // §12.12: the baselines captured at create ride the intent, so replay
    // restores the proof instead of re-running the checks.
    if let Some(value) = fields.get("baselines")
        && let Ok(baselines) = serde_json::from_value::<Vec<Option<Baseline>>>(value.clone())
    {
        set_baselines(plan, baselines);
    }
    // rung 4 rides the same way: frozen outputs are restored, never re-frozen.
    if let Some(value) = fields.get("snapshots")
        && let Ok(snapshots) = serde_json::from_value::<Vec<Option<Snapshot>>>(value.clone())
    {
        set_snapshots(plan, snapshots);
    }
    // check inputs ride with them: re-hashed at every verdict, never
    // re-frozen after the work starts.
    if let Some(value) = fields.get("inputs")
        && let Ok(inputs) = serde_json::from_value::<Vec<Vec<CheckInput>>>(value.clone())
    {
        set_inputs(plan, inputs);
    }
    // rung 5 rides with them: frozen shapes are restored, never re-read.
    if let Some(value) = fields.get("shapes")
        && let Ok(shapes) = serde_json::from_value::<Vec<Option<ShapeFreeze>>>(value.clone())
    {
        set_shapes(plan, shapes);
    }
    // the non-blocking checklist rides the create intent; the dispatcher
    // sets it post-create, so rebuild assigns it directly the same way
    if let Some(value) = fields.get("checklist")
        && let Ok(checklist) = serde_json::from_value::<Vec<String>>(value.clone())
    {
        plan.checklist = checklist;
    }
}

fn rebuild_created(
    root: &Path,
    sess: &str,
    seq: u64,
    fields: &serde_json::Map<String, serde_json::Value>,
) -> Option<Plan> {
    let get = |name: &str| fields.get(name);
    let goal = get("goal")?.as_str()?.to_string();
    let constraints = get("constraints")?
        .as_array()?
        .iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect();
    let acceptance = get("acceptance")?
        .as_array()?
        .iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect();
    let steps: Vec<NewStep> = serde_json::from_value(get("steps")?.clone()).ok()?;
    let budget_limit = get("budget_limit")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let mut plan = create(
        goal,
        constraints,
        acceptance,
        steps,
        budget_limit,
        &Limits::default(),
    )
    .ok()?;
    restore_create_riders(&mut plan, fields);
    plan.id = get("result_id")?.as_str()?.to_string();
    plan.created = get("result_created")?.as_str()?.to_string();
    plan.sessions = get("result_sessions")?
        .as_array()?
        .iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect();
    plan.applied_event = Some(scoped(sess, seq));
    store(root, &plan).ok()?;
    Some(plan)
}

/// Build the replacement plan an `accept_proposal` intent carries, without
/// touching the store or siblings. Shared by the orphan path and the
/// corrupt-file path so both construct the identical base.
fn build_fresh_from_accept(
    fields: &serde_json::Map<String, serde_json::Value>,
) -> Option<Plan> {
    let draft: PlanDraftArgs = serde_json::from_value(fields.get("draft")?.clone()).ok()?;
    let mut fresh = draft.build(u64::MAX, &Limits::default()).ok()?;
    // §12.12: same as create — the baselines captured on accept ride the
    // record, so replay never re-runs a check that has since changed.
    restore_create_riders(&mut fresh, fields);
    fresh.id = fields.get("new_id")?.as_str()?.to_string();
    fresh.created = fields.get("new_created")?.as_str()?.to_string();
    fresh.sessions = fields
        .get("new_sessions")?
        .as_array()?
        .iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect();
    Some(fresh)
}

/// Retire the plan an accept replaced — but only if it is still active.
/// Re-running abandonment is safe under this guard: an already-abandoned
/// sibling is left alone, so a rebuild can never resurrect or double-kill.
fn abandon_if_active(root: &Path, sess: &str, seq: u64, abandoned: &str) {
    if let Some(mut old) = read_plan_file(root, abandoned)
        && old.status == PlanStatus::Active
    {
        old.status = PlanStatus::Abandoned;
        old.revision = old.revision.saturating_add(1);
        old.applied_event = Some(scoped(sess, seq));
        let _ = store(root, &old);
    }
}

fn rebuild_accepted(
    root: &Path,
    sess: &str,
    seq: u64,
    fields: &serde_json::Map<String, serde_json::Value>,
) -> Option<Plan> {
    let mut fresh = build_fresh_from_accept(fields)?;
    if let Some(abandoned) = fields.get("abandoned").and_then(|value| value.as_str()) {
        abandon_if_active(root, sess, seq, abandoned);
    }
    fresh.applied_event = Some(scoped(sess, seq));
    store(root, &fresh).ok()?;
    Some(fresh)
}

/// Apply one journaled intent to an in-memory plan. `Ok(true)` means the op
/// took effect; `Ok(false)` means the record carries no replayable effect
/// (rejected op, read-only op, unknown shape); `Err` means the plan state
/// diverged from what the op was accepted against — the caller holds the
/// cursor there.
fn apply_record(
    plan: &mut Plan,
    record: &crate::agent::journal::Record,
) -> Result<bool, Rejection> {
    let fields = &record.fields;
    if fields.get("ok") == Some(&serde_json::Value::Bool(false)) {
        return Ok(false);
    }
    let op_name = fields.get("op").and_then(|value| value.as_str());
    match op_name {
        Some("verify") => {
            let index = fields
                .get("acceptance")
                .and_then(|value| value.as_u64())
                .ok_or_else(|| {
                    Rejection::new("replay_shape", "verify intent without acceptance index", "")
                })? as usize;
            let evidence: Vec<EvidenceRef> = fields
                .get("evidence_refs")
                .and_then(|value| serde_json::from_value(value.clone()).ok())
                .unwrap_or_default();
            // receipts ride the commit args so replay restores validation
            // without re-running checks; pre-receipt commits replay to the
            // legacy shape (status, no validation).
            let receipt: Option<Receipt> = fields
                .get("receipt")
                .and_then(|value| serde_json::from_value(value.clone()).ok());
            verify_acceptance(plan, index, evidence, false, receipt).map(|_| true)
        }
        Some("waive") => {
            let index = fields
                .get("index")
                .and_then(|value| value.as_u64())
                .ok_or_else(|| Rejection::new("replay_shape", "waive intent without index", ""))?
                as usize;
            let reason = fields
                .get("reason")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            waive(plan, index, reason).map(|_| true)
        }
        Some("waive_constraint") => {
            let index = fields
                .get("index")
                .and_then(|value| value.as_u64())
                .ok_or_else(|| {
                    Rejection::new("replay_shape", "waive_constraint intent without index", "")
                })? as usize;
            let reason = fields
                .get("reason")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            waive_constraint(plan, index, reason).map(|_| true)
        }
        Some("set_goal") => {
            let text = fields
                .get("text")
                .and_then(|value| value.as_str())
                .ok_or_else(|| Rejection::new("replay_shape", "goal intent without text", ""))?;
            let source = fields
                .get("source")
                .and_then(|value| value.as_str())
                .unwrap_or("user");
            let reason = fields
                .get("reason")
                .and_then(|value| value.as_str())
                .map(str::to_string);
            set_goal(plan, text.to_string(), source, reason);
            Ok(true)
        }
        Some("constraints") => {
            let action = fields
                .get("action")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let text = fields
                .get("text")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            match action {
                "add" => plan.constraints.push(text.to_string()),
                "remove" => {
                    if let Some(index) = plan.constraints.iter().position(|c| c == text) {
                        plan.constraints.remove(index);
                    }
                }
                _ => return Ok(false),
            }
            plan.revision = plan.revision.saturating_add(1);
            Ok(true)
        }
        Some("confirm") => {
            // replay pins the journaled confirmation; it never recomputes
            // digests or appends records (already accepted when journaled)
            let index = fields
                .get("index")
                .and_then(|value| value.as_u64())
                .ok_or_else(|| Rejection::new("replay_shape", "confirm intent without index", ""))?
                as usize;
            let reason = fields
                .get("reason")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let receipt: Receipt = fields
                .get("receipt")
                .and_then(|value| serde_json::from_value(value.clone()).ok())
                .ok_or_else(|| {
                    Rejection::new("replay_shape", "confirm intent without receipt", "")
                })?;
            attach_confirmation(plan, index, reason, receipt).map(|_| true)
        }
        Some("invalidate") => {
            let paths: Vec<String> = fields
                .get("paths")
                .and_then(|value| value.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            apply_invalidate(plan, &paths);
            Ok(true)
        }
        Some("flaky") => {
            let index = fields
                .get("index")
                .and_then(|value| value.as_u64())
                .ok_or_else(|| {
                    Rejection::new("replay_shape", "flaky intent without index", "")
                })? as usize;
            apply_flaky(plan, index);
            Ok(true)
        }
        Some("reopen") => {
            let ids: Vec<String> = fields
                .get("ids")
                .and_then(|value| value.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let reason = fields
                .get("reason")
                .and_then(|value| value.as_str())
                .unwrap_or("reopened by undo");
            for id in &ids {
                reopen_for_undo(plan, id, reason).map_err(|_| {
                    Rejection::new("replay_diverged", format!("cannot reopen step {id}"), "")
                })?;
            }
            Ok(true)
        }
        Some("add_acceptance") => {
            // items re-append (cursor discipline prevents double-apply, same
            // as Add); the proof captured at dispatch rides the record, so
            // replay restores it instead of re-running checks mid-work
            let items: Vec<String> = fields
                .get("items")
                .and_then(|value| value.as_array())
                .map(|array| {
                    array
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            add_acceptance(plan, items).map_err(|_| {
                Rejection::new("replay_diverged", "cannot re-add acceptance", "")
            })?;
            // proof vectors are probe-relative (new items only): they map
            // onto the last N acceptance items, which are exactly the ones
            // this intent appended (nothing ever removes acceptance items)
            if let Some(value) = fields.get("new_baselines")
                && let Ok(slots) = serde_json::from_value::<Vec<Option<Baseline>>>(value.clone())
            {
                let start = plan.acceptance.len().saturating_sub(slots.len());
                for (item, slot) in plan.acceptance.iter_mut().skip(start).zip(slots) {
                    if let Some(baseline) = slot {
                        item.baseline = Some(baseline);
                    }
                }
            }
            if let Some(value) = fields.get("new_snapshots")
                && let Ok(slots) = serde_json::from_value::<Vec<Option<Snapshot>>>(value.clone())
            {
                let start = plan.acceptance.len().saturating_sub(slots.len());
                for (item, slot) in plan.acceptance.iter_mut().skip(start).zip(slots) {
                    if let Some(snapshot) = slot {
                        item.snapshot = Some(snapshot);
                    }
                }
            }
            if let Some(value) = fields.get("new_shapes")
                && let Ok(slots) = serde_json::from_value::<Vec<Option<ShapeFreeze>>>(value.clone())
            {
                let start = plan.acceptance.len().saturating_sub(slots.len());
                for (item, slot) in plan.acceptance.iter_mut().skip(start).zip(slots) {
                    if let Some(shape) = slot {
                        item.shape = Some(shape);
                    }
                }
            }
            if let Some(value) = fields.get("new_inputs")
                && let Ok(slots) = serde_json::from_value::<Vec<Vec<CheckInput>>>(value.clone())
            {
                let start = plan.acceptance.len().saturating_sub(slots.len());
                for (item, slot) in plan.acceptance.iter_mut().skip(start).zip(slots) {
                    if !slot.is_empty() {
                        item.inputs = slot;
                    }
                }
            }
            Ok(true)
        }
        Some(
            "start" | "finish" | "block" | "unblock" | "cancel" | "add" | "split" | "complete"
            | "join" | "block_plan" | "propose_reset",
        ) => {
            let op: Op = serde_json::from_value(serde_json::Value::Object(fields.clone()))
                .map_err(|_| Rejection::new("replay_shape", "unparsable op intent", ""))?;
            apply(plan, op, &Limits::default(), None).map(|_| true)
        }
        _ => Ok(false),
    }
}

/// Atomic write: temp file + rename (§2.1.2).
pub fn store(root: &Path, plan: &Plan) -> Result<()> {
    let dir = plans_dir(root);
    std::fs::create_dir_all(&dir).context("creating plans directory")?;
    let text = serde_json::to_string_pretty(plan).context("encoding plan")?;
    // Unique tmp per writer: parallel subagents store the same plan file,
    // and a shared tmp name lets one writer's rename publish another's
    // half-written bytes (a torn read then quarantines a healthy plan).
    // Renames stay atomic, so the last whole write wins and readers never
    // see a half. Logical lost updates still heal from the journal.
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = dir.join(format!(
        "{}.{}.{}.json.tmp",
        plan.id,
        std::process::id(),
        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let target = dir.join(format!("{}.json", plan.id));
    std::fs::write(&tmp, text).context("writing plan")?;
    std::fs::rename(&tmp, &target).context("installing plan")?;
    Ok(())
}

/// Side-effect-free read of one plan file: missing or corrupt files yield
/// `None` instead of an error (unlike `open`, it never moves anything aside).
/// Used to resolve a session's linked plan without touching the store.
pub fn read_plan_file(root: &Path, id: &str) -> Option<Plan> {
    let text = std::fs::read_to_string(plans_dir(root).join(format!("{id}.json"))).ok()?;
    serde_json::from_str::<Plan>(&text).ok()
}

/// A plan that fails schema validation is moved aside, never silently dropped.
pub fn open(root: &Path, id: &str) -> Result<Plan> {
    let dir = plans_dir(root);
    let path = dir.join(format!("{id}.json"));
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading plan {id}"))?;
    match serde_json::from_str::<Plan>(&text) {
        Ok(plan) => Ok(plan),
        Err(e) => {
            // torn write or schema drift: rebuild from journaled intents
            // before giving up on the bytes (F1b). Only a clean rebuild
            // returns; anything doubtful quarantines below as before.
            if let Some(plan) = rebuild_corrupt(root, id) {
                return Ok(plan);
            }
            let corrupt = dir.join("corrupt");
            std::fs::create_dir_all(&corrupt).ok();
            let dest = corrupt.join(format!("{id}.json"));
            let _ = std::fs::rename(&path, &dest);
            Err(anyhow::anyhow!(
                "plan {id} failed schema validation ({e}); moved to {}",
                dest.display()
            ))
        }
    }
}

/// Preferred plan for the startup screen: the linked plan while it is
/// still active. A deleted, completed or abandoned id resolves to `None`
/// so the caller falls back to the global newest active plan instead of
/// showing a dead plan as active.
pub fn preferred_active_plan(root: &Path, id: &str) -> Option<Plan> {
    open(root, id)
        .ok()
        .filter(|plan| plan.status == PlanStatus::Active)
}

/// Rebuild a schema-broken plan file from journaled intents (§2.1.4).
/// Collects the birth intent (create, or accept_proposal for replacement
/// plans) plus every later op for this plan id across all session journals —
/// ordered best-effort by (ts, session, seq), because there is no
/// total-order counter — and re-applies them onto a fresh base. Any
/// Rejection, a missing birth intent, or a deliberate `plan_deleted` aborts
/// to `None` and the caller quarantines the bytes. Sibling retirement on the
/// accept path reuses the orphan guard (only a still-active sibling moves),
/// so a rebuild can neither resurrect nor double-kill.
fn rebuild_corrupt(root: &Path, id: &str) -> Option<Plan> {
    // (ts, session, seq, fields) over every session journal
    let mut records: Vec<(
        String,
        String,
        u64,
        serde_json::Map<String, serde_json::Value>,
    )> = Vec::new();
    let dir = root.join(".sqwai").join("journal");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return None;
    };
    for entry in entries.flatten() {
        if entry.path().extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(sess) = entry
            .path()
            .file_stem()
            .and_then(|name| name.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        for line in text.lines() {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if value.get("kind").and_then(|k| k.as_str()) != Some("plan") {
                continue;
            }
            let seq = value.get("seq").and_then(|s| s.as_u64()).unwrap_or(0);
            let ts = value
                .get("ts")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string();
            let fields = value.as_object().cloned().unwrap_or_default();
            records.push((ts, sess.clone(), seq, fields));
        }
    }
    // deliberate absence wins over any rebuild
    let deleted = records.iter().any(|(_, _, _, fields)| {
        fields.get("op").and_then(|o| o.as_str()) == Some("plan_deleted")
            && fields.get("plan_id").and_then(|p| p.as_str()) == Some(id)
    });
    if deleted {
        return None;
    }
    records.sort_by(|a, b| (&a.0, &a.1, a.2).cmp(&(&b.0, &b.1, b.2)));
    // the birth intent this file was born from: a create (mirrors
    // rebuild_created's field parsing), or an accept_proposal whose new_id
    // is this plan (mirrors rebuild_accepted). Kept local so orphan
    // handling stays untouched.
    enum Birth {
        Created(usize),
        Accepted(usize),
    }
    let birth = records
        .iter()
        .position(|(_, _, _, fields)| {
            fields.get("op").and_then(|o| o.as_str()) == Some("create")
                && fields.get("result_id").and_then(|r| r.as_str()) == Some(id)
        })
        .map(Birth::Created)
        .or_else(|| {
            records
                .iter()
                .position(|(_, _, _, fields)| {
                    fields.get("op").and_then(|o| o.as_str()) == Some("accept_proposal")
                        && fields.get("new_id").and_then(|r| r.as_str()) == Some(id)
                })
                .map(Birth::Accepted)
        })?;
    let (birth_idx, mut plan) = match birth {
        Birth::Created(create_idx) => {
            let (_, create_sess, create_seq, create_fields) = records[create_idx].clone();
            let mut plan = create(
                create_fields.get("goal")?.as_str()?.to_string(),
                create_fields
                    .get("constraints")?
                    .as_array()?
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect(),
                create_fields
                    .get("acceptance")?
                    .as_array()?
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect(),
                serde_json::from_value(create_fields.get("steps")?.clone()).ok()?,
                create_fields
                    .get("budget_limit")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                &Limits::default(),
            )
            .ok()?;
            plan.id = id.to_string();
            plan.created = create_fields.get("result_created")?.as_str()?.to_string();
            plan.sessions = create_fields
                .get("result_sessions")?
                .as_array()?
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            plan.applied_event = Some(scoped(&create_sess, create_seq));
            plan.applied_events.insert(create_sess.clone(), create_seq);
            // the frozen riders ride the create intent here exactly like on
            // the orphan path — without them the rebuilt plan re-runs
            // proven checks
            restore_create_riders(&mut plan, &create_fields);
            (create_idx, plan)
        }
        Birth::Accepted(accept_idx) => {
            let (_, accept_sess, accept_seq, accept_fields) = records[accept_idx].clone();
            let mut plan = build_fresh_from_accept(&accept_fields)?;
            if let Some(abandoned) = accept_fields
                .get("abandoned")
                .and_then(|value| value.as_str())
            {
                abandon_if_active(root, &accept_sess, accept_seq, abandoned);
            }
            plan.applied_event = Some(scoped(&accept_sess, accept_seq));
            plan.applied_events
                .insert(accept_sess.clone(), accept_seq);
            (accept_idx, plan)
        }
    };
    // every later op for this plan, in best-effort global order
    for (idx, (ts, sess, seq, fields)) in records.iter().enumerate() {
        if idx == birth_idx {
            continue;
        }
        let mine = fields.get("plan_id").and_then(|p| p.as_str()) == Some(id);
        if !mine {
            continue;
        }
        let after_birth = (ts.as_str(), sess.as_str(), *seq)
            > (
                records[birth_idx].0.as_str(),
                records[birth_idx].1.as_str(),
                records[birth_idx].2,
            );
        if !after_birth {
            continue;
        }
        let record = crate::agent::journal::Record {
            seq: *seq,
            ts: ts.clone(),
            session: sess.clone(),
            step: None,
            plan: None,
            agent: String::new(),
            epoch: None,
            kind: "plan".to_string(),
            fields: fields.clone(),
        };
        match apply_record(&mut plan, &record) {
            Ok(true) => {
                plan.applied_event = Some(scoped(sess, *seq));
                plan.applied_events.insert(sess.clone(), *seq);
            }
            Ok(false) => {}
            Err(_) => return None,
        }
    }
    store(root, &plan).ok()?;
    Some(plan)
}

/// At most one active plan per project (§2.1.1).
pub fn open_active(root: &Path) -> Result<Option<Plan>> {
    open_active_for_session(root, None)
}

/// Open the active plan for a specific session: the session's own active
/// plan, or — when the session has none — the most recent active plan.
///
/// The fallback preserves single-plan behavior for views (a fresh session
/// sees the one active plan). Anything that mutates shared state or
/// permanently links identity must use [`open_own_active_plan`] instead —
/// silently adopting a foreign plan pollutes other sessions (#171).
pub fn open_active_for_session(root: &Path, session_id: Option<&str>) -> Result<Option<Plan>> {
    let dir = plans_dir(root);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };
    let mut active_plans: Vec<Plan> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(plan) = serde_json::from_str::<Plan>(&text)
            && plan.status == PlanStatus::Active
        {
            active_plans.push(plan);
        }
    }
    if active_plans.is_empty() {
        return Ok(None);
    }
    if let Some(sid) = session_id
        && let Some(plan) = active_plans
            .iter()
            .find(|p| p.sessions.iter().any(|s| s == sid))
    {
        return Ok(Some(plan.clone()));
    } else if session_id.is_some() {
        // #171: a session without its own active plan gets None — falling
        // through to another session's plan here silently adopts foreign
        // work into this session's identity and attribution.
        return Ok(None);
    }
    // Deterministic fallback: pick the most recent active plan by created timestamp
    active_plans.sort_by(|a, b| b.created.cmp(&a.created));
    Ok(active_plans.into_iter().next())
}

pub fn list(root: &Path) -> Vec<Plan> {
    let dir = plans_dir(root);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|t| serde_json::from_str::<Plan>(&t).ok())
        .collect()
}

pub fn list_active(root: &Path) -> Vec<Plan> {
    list(root)
        .into_iter()
        .filter(|p| p.status == PlanStatus::Active)
        .collect()
}

