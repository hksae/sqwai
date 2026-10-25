use super::{
    EvidenceRef, Limits, NewStep, Op, Plan, PlanStatus, Rejection, apply, create, plans_dir,
    reopen_for_undo,
};
use anyhow::{Context, Result};
use std::path::Path;

/// Cross-process arbitration for plan-file writers (audit C3). Under
/// `--force` two processes can load→append→store the same plan; the later
/// store regressed the earlier one's content and cursor, and replay then
/// stalled on the re-apply conflict. An exclusive lock file (create-only,
/// PID-stale pruned, same pattern as the project lock) serializes the
/// journal-append + store pair; [`commit`] additionally refuses when the
/// file moved since the caller loaded it.
pub(crate) struct PlanLock {
    path: std::path::PathBuf,
}

impl PlanLock {
    pub(crate) fn acquire(root: &Path, plan_id: &str) -> Result<Self> {
        let path = plans_dir(root).join(format!("{plan_id}.lock"));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        for attempt in 0..300u32 {
            use std::io::Write;
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    let _ = write!(file, "{}", std::process::id());
                    return Ok(Self { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if Self::holder_dead(&path) {
                        std::fs::remove_file(&path).ok();
                        continue;
                    }
                    if attempt == 299 {
                        anyhow::bail!("plan lock busy: another process is committing to {plan_id}");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => return Err(e).with_context(|| "creating plan lock"),
            }
        }
        unreachable!("the attempt loop always returns or bails")
    }

    fn holder_dead(path: &Path) -> bool {
        let stale_by_age = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map(|mtime| {
                mtime
                    .elapsed()
                    .is_ok_and(|age| age > std::time::Duration::from_secs(60))
            })
            .unwrap_or(false);
        if stale_by_age {
            return true;
        }
        match std::fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
        {
            Some(pid) => pid != std::process::id() && !crate::lock::is_pid_alive(pid),
            // unreadable/garbage holder: let the age rule decide, do not
            // steal a lock we cannot attribute
            None => false,
        }
    }
}

impl Drop for PlanLock {
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).ok();
    }
}

/// Immutable spawn context a subagent inherits: which plan step it
/// works on and at which epoch. Mutations and evidence from an older epoch
/// are refused / filtered after the step is reopened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepContext {
    pub plan_id: String,
    pub step_id: String,
    pub step_epoch: u64,
}

// ---------------------------------------------------------------- journal-first commits and replay
//
// Every mutation of a plan file is preceded by a journal `plan` intent
// record carrying the full op args. The record's seq becomes the file's
// per-session cursor (`applied_events`) in the same store, so a crash
// between the two is detectable: on load the host replays journal events
// after the cursor. Replay is pure re-application — no commands run, no
// approvals, no evidence gates — which is sound because every replayed op
// was already accepted (and validated, where validation applies) when
// first journaled.

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
    // Arbitration (audit C3): serialize writers on this plan file and
    // refuse a commit whose base is stale — a store over a newer on-disk
    // state regresses both content and cursors, and the replay cannot
    // always re-apply (single-active-step makes Start/Start conflicts
    // unrecoverable, stranding the stream in `stalled`).
    let _lock = PlanLock::acquire(root, &plan.id)?;
    if let Some(disk) = read_plan_file(root, &plan.id) {
        let moved = disk
            .applied_events
            .iter()
            .any(|(sess, seq)| plan.applied_events.get(sess).is_none_or(|mine| mine < seq));
        if moved {
            anyhow::bail!(
                "plan_moved: another session committed to {} while this operation was in flight — re-read the plan and retry",
                plan.id
            );
        }
    }
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
/// re-attach evidence refs whose file update was lost. Files without any
/// cursor (never committed through journal-first) are skipped.
/// Idempotent: a clean tree changes nothing and stores nothing.
pub fn replay(root: &Path) -> Result<ReplayReport> {
    let mut report = ReplayReport::default();
    for plan in list(root) {
        // One entry per session that ever committed here. BTreeMap order is
        // deterministic; each stream heals independently (a stall holds its
        // own stream while the others keep healing).
        let cursors: std::collections::BTreeMap<String, u64> = plan.applied_events.clone();
        if cursors.is_empty() {
            continue;
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
                if matches!(record.kind.as_str(), "tool_result" | "file_diff")
                    && (record.kind != "file_diff"
                        || crate::agent::journal::file_diff_is_plan_evidence(&record.fields))
                    && record.plan.as_deref() == Some(plan.id.as_str())
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
    plan.applied_events
        .into_iter()
        .max_by_key(|(_, seq)| *seq)
        .map(|(sess, _)| sess)
}

/// Rebuild plans whose create intent was journaled but whose file is
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
            if fields.get("op").and_then(|value| value.as_str()) == Some("create") {
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
        }
    }
    rebuilt
}

/// Done-criteria on a create intent: the plain-text notes, merged
/// with the legacy non-blocking checklist so an old rebuild lands the same
/// criteria the live plan carried. Shared by the orphan and corrupt paths.
fn restore_create_criteria(plan: &mut Plan, fields: &serde_json::Map<String, serde_json::Value>) {
    let list = |name: &str| -> Vec<String> {
        fields
            .get(name)
            .and_then(|value| value.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|value| value.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut criteria = list("criteria");
    if criteria.is_empty() {
        criteria = list("acceptance");
    }
    criteria.extend(list("checklist"));
    plan.criteria = criteria;
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
    let criteria = get("criteria")
        .or_else(|| get("acceptance"))?
        .as_array()?
        .iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect();
    let steps: Vec<NewStep> = serde_json::from_value(get("steps")?.clone()).ok()?;
    let mut plan = create(
        goal,
        constraints,
        criteria,
        steps,
        &Limits::default(),
    )
    .ok()?;
    restore_create_criteria(&mut plan, fields);
    plan.id = get("result_id")?.as_str()?.to_string();
    plan.created = get("result_created")?.as_str()?.to_string();
    plan.sessions = get("result_sessions")?
        .as_array()?
        .iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect();
    plan.applied_events.insert(sess.to_string(), seq);
    store(root, &plan).ok()?;
    Some(plan)
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
        Some("redo_steps") => {
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
            for id in &ids {
                if let Some(step) = plan.step_mut(id) {
                    step.status = super::StepStatus::Done;
                    step.reason = None;
                    if step.finished.is_none() {
                        step.finished = Some(super::now());
                    }
                }
            }
            super::ops::auto_advance(plan);
            if plan
                .steps
                .iter()
                .all(|s| matches!(s.status, super::StepStatus::Done | super::StepStatus::Cancelled))
            {
                plan.status = PlanStatus::Completed;
            }
            plan.revision = plan.revision.saturating_add(1);
            Ok(true)
        }
        Some("finish" | "cancel" | "add" | "join" | "abandon" | "propose_reset" | "block_plan") => {
            let op: Op = serde_json::from_value(serde_json::Value::Object(fields.clone()))
                .map_err(|_| Rejection::new("replay_shape", "unparsable op intent", ""))?;
            apply(plan, op, &Limits::default()).map(|_| true)
        }
        _ => Ok(false),
    }
}

/// Atomic write: temp file + rename.
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
/// Files from a newer binary are not adopted: they read as absent here and
/// are left in place, never downgraded.
pub fn read_plan_file(root: &Path, id: &str) -> Option<Plan> {
    let text = std::fs::read_to_string(plans_dir(root).join(format!("{id}.json"))).ok()?;
    serde_json::from_str::<Plan>(&text)
        .ok()
        .filter(|plan| plan.version <= super::PLAN_VERSION)
}

/// A plan that fails schema validation is moved aside, never silently dropped.
/// A well-formed file from a newer binary is refused without moving it:
/// rebuilding it through this binary's journal intents would silently
/// downgrade fields the old code does not know.
pub fn open(root: &Path, id: &str) -> Result<Plan> {
    let dir = plans_dir(root);
    let path = dir.join(format!("{id}.json"));
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading plan {id}"))?;
    match serde_json::from_str::<Plan>(&text) {
        Ok(plan) => {
            if plan.version > super::PLAN_VERSION {
                return Err(anyhow::anyhow!(
                    "plan {id} is version {}, this binary reads up to version {} — left in place",
                    plan.version,
                    super::PLAN_VERSION,
                ));
            }
            Ok(plan)
        }
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

/// Rebuild a schema-broken plan file from journaled intents.
/// Collects the create intent plus every later op for this plan id across all
/// session journals — ordered best-effort by (ts, session, seq), because there
/// is no total-order counter — and re-applies them onto a fresh base. Any
/// Rejection, a missing create intent, or a deliberate `plan_deleted` aborts
/// to `None` and the caller quarantines the bytes.
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
    // rebuild_created's field parsing). Kept local so orphan handling stays
    // untouched.
    let create_idx = records.iter().position(|(_, _, _, fields)| {
        fields.get("op").and_then(|o| o.as_str()) == Some("create")
            && fields.get("result_id").and_then(|r| r.as_str()) == Some(id)
    })?;
    let (birth_idx, mut plan) = {
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
                .get("criteria")
                .or_else(|| create_fields.get("acceptance"))?
                .as_array()?
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            serde_json::from_value(create_fields.get("steps")?.clone()).ok()?,
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
        plan.applied_events.insert(create_sess.clone(), create_seq);
        restore_create_criteria(&mut plan, &create_fields);
        (create_idx, plan)
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
                plan.applied_events.insert(sess.clone(), *seq);
            }
            Ok(false) => {}
            Err(_) => return None,
        }
    }
    store(root, &plan).ok()?;
    Some(plan)
}

/// At most one active plan per project.
#[cfg(test)]
pub fn open_active(root: &Path) -> Result<Option<Plan>> {
    open_active_for_session(root, None)
}

static PLAN_META_CACHE: std::sync::LazyLock<std::sync::Mutex<PlanMetaCache>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(PlanMetaCache::default()));

/// Directory-scan key: (plan id, status, created, sessions-hash). The journal
/// replay only matters for content accuracy, never for "which plans are
/// active" — status flips only through a journaled intent, and the file moves
/// with the intent in the same commit, so metadata read from the file is
/// current exactly when the file is.
#[derive(Debug, Clone)]
struct ActivePlanMeta {
    id: String,
    created: String,
    sessions: Vec<String>,
    modified: Option<std::time::SystemTime>,
    len: u64,
}

/// Cache over `plans_dir`: for `open_active_for_session` the identity and
/// membership of every plan is what matters, not its full content. Entries
/// are validated against the file's (mtime, size) on every read: a same
/// (mtime, size) hits without touching serde_json; a moved file re-parses
/// exactly that file; vanished files and new files are reflected from the
/// directory listing. A hand-edited file changes mtime or size, so it can
/// never poison the cache — the new bytes are read before anyone decides
/// whether the plan is active.
#[derive(Debug, Default)]
struct PlanMetaCache {
    root: Option<std::path::PathBuf>,
    entries: std::collections::HashMap<String, ActivePlanMeta>,
}

impl PlanMetaCache {
    fn active_meta(&mut self, root: &Path) -> Vec<ActivePlanMeta> {
        if self.root.as_ref() != Some(&root.to_path_buf()) {
            self.root = Some(root.to_path_buf());
            self.entries.clear();
        }
        let dir = plans_dir(root);
        let mut live: std::collections::HashSet<String> = std::collections::HashSet::new();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            self.entries.clear();
            return Vec::new();
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            live.insert(id.clone());
            let Ok(meta) = std::fs::metadata(&path) else {
                self.entries.remove(&id);
                continue;
            };
            let fingerprint = (meta.modified().ok(), meta.len());
            let fresh = self.entries.get(&id).is_some_and(|cached| {
                cached.modified == fingerprint.0 && cached.len == fingerprint.1
            });
            if fresh {
                continue;
            }
            match parse_active_meta(&path) {
                Some(m) if m.modified == fingerprint.0 && m.len == fingerprint.1 => {
                    self.entries.insert(id, m);
                }
                _ => {
                    self.entries.remove(&id);
                }
            }
        }
        self.entries.retain(|id, _| live.contains(id));
        self.entries.values().cloned().collect()
    }
}

/// Parse only (id, status, created, sessions, version) from a plan file.
/// A file that fails validation — or comes from a newer binary — is not
/// `active` here. Whether an unreadable file later quarantines is the
/// caller's decision in `open`, never in this directory scan; future
/// versions are refused there without moving the file.
fn parse_active_meta(path: &Path) -> Option<ActivePlanMeta> {
    use serde::de::Deserializer as _;
    struct MetaVisitor;
    impl<'de> serde::de::Visitor<'de> for MetaVisitor {
        type Value = Option<ActivePlanMeta>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a plan object with id/status/created/sessions/version")
        }
        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let mut id: Option<String> = None;
            let mut status: Option<String> = None;
            let mut created: Option<String> = None;
            let mut sessions: Vec<String> = Vec::new();
            let mut version: Option<u32> = None;
            while let Some(key) = map.next_key::<String>()? {
                match key.as_str() {
                    "id" => id = map.next_value().ok(),
                    "status" => status = map.next_value().ok(),
                    "created" => created = map.next_value().ok(),
                    "sessions" => sessions = map.next_value().unwrap_or_default(),
                    "version" => version = map.next_value().ok(),
                    _ => {
                        let _: serde::de::IgnoredAny = map.next_value()?;
                    }
                }
            }
            let (id, status, created, version) = match (id, status, created, version) {
                (Some(id), Some(status), Some(created), Some(version)) => {
                    (id, status, created, version)
                }
                _ => return Ok(None),
            };
            if status != "active" || version > super::PLAN_VERSION {
                return Ok(None);
            }
            Ok(Some(ActivePlanMeta {
                id,
                created,
                sessions,
                modified: None,
                len: 0,
            }))
        }
    }
    let text = std::fs::read_to_string(path).ok()?;
    let meta = serde_json::Deserializer::from_str(&text)
        .deserialize_map(MetaVisitor)
        .ok()??;
    let file_meta = std::fs::metadata(path).ok()?;
    Some(ActivePlanMeta {
        modified: file_meta.modified().ok(),
        len: file_meta.len(),
        ..meta
    })
}

/// Open the active plan for a specific session: the session's own active
/// plan, or — when the session has none — the most recent active plan.
///
/// The fallback preserves single-plan behavior for views (a fresh session
/// sees the one active plan). Anything that mutates shared state or
/// permanently links identity must use [`open_own_active_plan`] instead —
/// silently adopting a foreign plan pollutes other sessions (#171).
///
/// Identity scan is cached: the directory listing plus each file's
/// (mtime, size) decide whether its metadata is fresh, so a hot loop over
/// one unchanged project never re-parses JSON. The full plan body is parsed
/// only for the one selected plan, and only in a caller that needs it.
pub fn open_active_for_session(root: &Path, session_id: Option<&str>) -> Result<Option<Plan>> {
    let active_meta: Vec<ActivePlanMeta> = PLAN_META_CACHE
        .lock()
        .map(|mut cache| cache.active_meta(root))
        .unwrap_or_default();
    if active_meta.is_empty() {
        return Ok(None);
    }
    if let Some(sid) = session_id
        && let Some(meta) = active_meta
            .iter()
            .find(|m| m.sessions.iter().any(|s| s == sid))
    {
        // #171: a session without its own active plan gets None — falling
        // through to another session's plan here silently adopts foreign
        // work into this session's identity and attribution.
        return open(root, &meta.id).map(Some);
    } else if session_id.is_some() {
        return Ok(None);
    }
    // Deterministic fallback: pick the most recent active plan by created
    // timestamp. Compared as text (#182) the order only holds while every stamp
    // carries the same UTC offset — across a DST change `01:00+02:00` sorts
    // after `02:00+03:00` although it is an hour later, so the "newest" plan
    // can be the older one. Compare instants; an unparseable stamp loses to
    // any real one, which keeps the comparator a total order.
    let instant = |created: &str| {
        chrono::DateTime::parse_from_rfc3339(created)
            .map(|t| t.with_timezone(&chrono::Utc))
            .ok()
    };
    let mut ordered = active_meta;
    ordered.sort_by(|a, b| match (instant(&a.created), instant(&b.created)) {
        (Some(x), Some(y)) => y.cmp(&x),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => b.created.cmp(&a.created),
    });
    ordered.first().map(|m| open(root, &m.id)).transpose()
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
        .filter(|p| p.version <= super::PLAN_VERSION)
        .collect()
}

pub fn list_active(root: &Path) -> Vec<Plan> {
    list(root)
        .into_iter()
        .filter(|p| p.status == PlanStatus::Active)
        .collect()
}
