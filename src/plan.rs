//! Structured plan (DESIGN §2.1).
//!
//! A plan is a host-owned document: the model reaches it only through the
//! `plan` tool operations validated here. The model can never write `goal`,
//! `constraints`, `acceptance[].status`, `evidence` or `folded` directly.
//!

use anyhow::{Context, Result};
use chrono::Local;
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// A host-owned journal reference. The session is part of the identity because
/// journal sequence numbers restart for every session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvidenceRef {
    pub session: String,
    pub seq: u64,
}

impl<'de> Deserialize<'de> for EvidenceRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct EvidenceRefVisitor;
        impl<'de> Visitor<'de> for EvidenceRefVisitor {
            type Value = EvidenceRef;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("an evidence object or legacy sequence number")
            }

            fn visit_u64<E>(self, seq: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(EvidenceRef {
                    session: String::new(),
                    seq,
                })
            }

            fn visit_map<M>(self, map: M) -> Result<Self::Value, M::Error>
            where
                M: de::MapAccess<'de>,
            {
                #[derive(Deserialize)]
                struct Wire {
                    session: String,
                    seq: u64,
                }
                Wire::deserialize(de::value::MapAccessDeserializer::new(map)).map(|wire| {
                    EvidenceRef {
                        session: wire.session,
                        seq: wire.seq,
                    }
                })
            }
        }
        deserializer.deserialize_any(EvidenceRefVisitor)
    }
}

/// Default ceiling for the number of steps (§2.1.2, `[plan].max_steps`).
pub const MAX_STEPS_DEFAULT: usize = 24;

// ---------------------------------------------------------------- model

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Active,
    Completed,
    Abandoned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    InProgress,
    Blocked,
    Done,
    Cancelled,
    Reopened,
}

impl StepStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
            Self::Reopened => "reopened",
        }
    }

    /// Steps the host may fold away when the plan outgrows its budget (§2.1.5).
    #[allow(dead_code)]
    pub fn is_closed(self) -> bool {
        matches!(self, Self::Done | Self::Cancelled)
    }

    #[allow(dead_code)]
    pub fn is_open(self) -> bool {
        !self.is_closed()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Research,
    Change,
    Verify,
}

impl StepKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Research => "research",
            Self::Change => "change",
            Self::Verify => "verify",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceStatus {
    Pending,
    /// A host-run check passed for this item (§2.1.4). Old files say
    /// `verified`; that stays readable through the alias.
    #[serde(alias = "verified")]
    Passed,
    Waived,
}

impl AcceptanceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Passed => "passed",
            Self::Waived => "waived",
        }
    }
}

/// Validation state of a step or acceptance item, separate from whether the
/// work was performed (§2.1.2, §2.1.4). `finish` moves a step to `done` and
/// never touches this; only a host-recorded `verification_receipt` sets
/// `passed`, and later state changes flip it to `stale`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ValidationStatus {
    #[default]
    Pending,
    Passed,
    Stale,
    Waived,
}

impl ValidationStatus {
    /// Text form for the anchor and `/plan` (wired in phase 3/5).
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Passed => "passed",
            Self::Stale => "stale",
            Self::Waived => "waived",
        }
    }
}

/// One verification run bound to the exact state it checked (§2.1.4).
/// `session`/`seq` point at the journal `verification_receipt` record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Receipt {
    pub session: String,
    pub seq: u64,
    pub state_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    pub at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Validation {
    #[serde(default)]
    pub status: ValidationStatus,
    #[serde(default)]
    pub receipts: Vec<Receipt>,
}

/// What a step intends to touch (§2.1.2, §2.4.8). `modify` and `remove`
/// refer to existing code; `create` declares a new path/symbol that must
/// not exist yet. Plain strings stay accepted on input and mean
/// `modify` (the `path::symbol` tail, if any, becomes `symbol`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RefIntent {
    #[default]
    Modify,
    Create,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StepRef {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(default)]
    pub intent: RefIntent,
}

impl<'de> Deserialize<'de> for StepRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StepRefVisitor;
        impl<'de> Visitor<'de> for StepRefVisitor {
            type Value = StepRef;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a ref object or a plain \"path[::symbol]\" string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StepRef::from(value))
            }

            fn visit_map<M>(self, map: M) -> Result<Self::Value, M::Error>
            where
                M: de::MapAccess<'de>,
            {
                #[derive(Deserialize)]
                struct Wire {
                    path: String,
                    #[serde(default)]
                    symbol: Option<String>,
                    #[serde(default)]
                    intent: RefIntent,
                }
                Wire::deserialize(de::value::MapAccessDeserializer::new(map)).map(|wire| {
                    StepRef {
                        path: wire.path,
                        symbol: wire.symbol,
                        intent: wire.intent,
                    }
                })
            }
        }
        deserializer.deserialize_any(StepRefVisitor)
    }
}

impl From<&str> for StepRef {
    /// `"src/x.rs"` → modify `src/x.rs`; `"src/x.rs::fn::foo"` → modify
    /// path `src/x.rs`, symbol `fn::foo` (the `::` convention the
    /// misattribution warning already splits on).
    fn from(value: &str) -> Self {
        match value.split_once("::") {
            Some((path, symbol)) if !path.is_empty() && !symbol.is_empty() => StepRef {
                path: path.to_string(),
                symbol: Some(symbol.to_string()),
                intent: RefIntent::Modify,
            },
            _ => StepRef {
                path: value.to_string(),
                symbol: None,
                intent: RefIntent::Modify,
            },
        }
    }
}

impl From<String> for StepRef {
    fn from(value: String) -> Self {
        StepRef::from(value.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalRevision {
    pub text: String,
    pub source: String,
    pub at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub text: String,
    pub source: String,
    pub created: String,
    #[serde(default)]
    pub history: Vec<GoalRevision>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Acceptance {
    pub text: String,
    pub status: AcceptanceStatus,
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
    #[serde(default)]
    pub validation: Validation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// How the host is meant to settle an acceptance item (§2.1.2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AcceptanceKind<'a> {
    /// `cmd: <command>` — the host runs it and the result is the verification
    Command(&'a str),
    /// `manual: <text>` — no command can settle it; the user waives it
    Manual(&'a str),
    /// free text — settled by host-recorded evidence from a verify step
    Text(&'a str),
}

impl Acceptance {
    /// Classify by prefix. Unprefixed text is `Text`, per §2.1.2.
    pub fn kind(&self) -> AcceptanceKind<'_> {
        let text = self.text.trim();
        if let Some(command) = text.strip_prefix("cmd:") {
            AcceptanceKind::Command(command.trim())
        } else if let Some(rest) = text.strip_prefix("manual:") {
            AcceptanceKind::Manual(rest.trim())
        } else {
            AcceptanceKind::Text(text)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub title: String,
    #[serde(default = "default_kind")]
    pub kind: StepKind,
    pub status: StepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Journal seq values. Written by the host only (§2.1.2).
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
    /// What the step intends to touch (§2.1.2, §2.4.8).
    #[serde(default)]
    pub refs: Vec<StepRef>,
    /// Check state, separate from step status (§2.1.2). `finish` never
    /// writes this; the host sets `passed` via receipts (phase 3).
    #[serde(default)]
    pub validation: Validation,
    /// Bumped by every host-only reopen; subagent evidence from an older
    /// epoch does not count (§2.2.4, phase 2).
    #[serde(default)]
    pub step_epoch: u64,
    /// Set on pending steps after a goal revision (§2.1.6).
    /// Set on pending steps after a goal revision (§2.1.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_goal: Option<bool>,
}

fn default_kind() -> StepKind {
    StepKind::Change
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Folded {
    pub ids: Vec<String>,
    pub text: String,
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Budget {
    pub tokens: u64,
    pub limit: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub version: u32,
    pub id: String,
    pub status: PlanStatus,
    pub created: String,
    #[serde(default)]
    pub sessions: Vec<String>,
    /// Scoped `session:seq` of the last journal event applied to this file.
    /// The plan is a projection replayed from here on load (§2.1.4, phase 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_event: Option<String>,
    pub goal: Goal,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub acceptance: Vec<Acceptance>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub folded: Vec<Folded>,
    pub budget: Budget,
    pub revision: u64,
    #[serde(default)]
    pub rejections_in_a_row: u32,
}

impl Plan {
    pub fn step(&self, id: &str) -> Option<&Step> {
        self.steps.iter().find(|s| s.id == id)
    }

    pub fn step_mut(&mut self, id: &str) -> Option<&mut Step> {
        self.steps.iter_mut().find(|s| s.id == id)
    }

    pub fn counts(&self) -> StepCounts {
        let mut c = StepCounts::default();
        for s in &self.steps {
            match s.status {
                StepStatus::Pending => c.pending += 1,
                StepStatus::InProgress => c.in_progress += 1,
                StepStatus::Blocked => c.blocked += 1,
                StepStatus::Done => c.done += 1,
                StepStatus::Cancelled => c.cancelled += 1,
                StepStatus::Reopened => c.reopened += 1,
            }
        }
        c
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct StepCounts {
    pub pending: usize,
    pub in_progress: usize,
    pub blocked: usize,
    pub done: usize,
    pub cancelled: usize,
    pub reopened: usize,
}

// ---------------------------------------------------------------- storage

pub fn plans_dir(root: &Path) -> PathBuf {
    root.join(".sqwai").join("plans")
}

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// ULID: 48-bit millisecond timestamp + 80 random bits, Crockford base32.
pub fn new_id() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
        & 0xFFFF_FFFF_FFFF;
    let bytes = *Uuid::new_v4().as_bytes();
    let mut rand: u128 = 0;
    for b in &bytes[..10] {
        rand = (rand << 8) | *b as u128;
    }
    let mut out = String::with_capacity(26);
    for i in 0..10 {
        let shift = 45 - 5 * i;
        out.push(CROCKFORD[((ms >> shift) & 31) as usize] as char);
    }
    for i in 0..16 {
        let shift = 75 - 5 * i;
        out.push(CROCKFORD[((rand >> shift) & 31) as usize] as char);
    }
    out
}

fn now() -> String {
    Local::now().to_rfc3339()
}

/// Reopen a completed step after host-side undo removed its recorded evidence.
/// Conservative rule (§3.6): ANY reverted part of the step's result reopens
/// it — reverting a subset cannot leave the step marked completed. Bumps
/// `step_epoch` so subagent records from before the reopen stop counting as
/// evidence, and resets `validation` (a reverted result is unverified).
pub fn reopen_for_undo(plan: &mut Plan, step_id: &str, reason: impl Into<String>) -> Result<()> {
    let step = plan
        .step_mut(step_id)
        .ok_or_else(|| anyhow::anyhow!("unknown plan step {step_id}"))?;
    if step.status != StepStatus::Done {
        return Ok(());
    }
    step.status = StepStatus::Reopened;
    step.finished = None;
    step.summary = None;
    step.evidence.clear();
    step.validation = Validation::default();
    step.step_epoch = step.step_epoch.saturating_add(1);
    step.reason = Some(reason.into());
    plan.revision = plan.revision.saturating_add(1);
    Ok(())
}

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
    fields.insert(
        "op".to_string(),
        serde_json::Value::String(op.to_string()),
    );
    fields.insert(
        "plan_id".to_string(),
        serde_json::Value::String(plan.id.clone()),
    );
    fields.insert(
        "by".to_string(),
        serde_json::Value::String(by.to_string()),
    );
    fields.insert("ok".to_string(), serde_json::Value::Bool(ok));
    let mut journal = crate::agent::journal::Journal::open(root, session_id)?;
    let seq = journal.append("plan", serde_json::Value::Object(fields))?;
    plan.applied_event = Some(scoped(session_id, seq));
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
        let Some((sess, cursor)) = split_applied(&plan.applied_event) else {
            continue;
        };
        let mut records = match crate::agent::journal::Journal::records_for(root, &sess) {
            Ok(records) => records,
            Err(_) => continue,
        };
        records.sort_by_key(|record| record.seq);
        let mut plan = plan;
        let mut dirty = false;
        for record in records.iter().filter(|record| record.seq > cursor) {
            if record.kind == "plan"
                && record
                    .fields
                    .get("plan_id")
                    .and_then(|value| value.as_str())
                    == Some(plan.id.as_str())
            {
                match apply_record(&mut plan, record) {
                    Ok(true) => {
                        plan.applied_event = Some(scoped(&sess, record.seq));
                        store(root, &plan)?;
                        report.ops_applied += 1;
                        if !report.plans_healed.contains(&plan.id) {
                            report.plans_healed.push(plan.id.clone());
                        }
                    }
                    Ok(false) => {}
                    Err(_) => {
                        // State diverged from what the op was accepted against;
                        // hold the cursor and leave the rest for a human.
                        if !report.stalled.contains(&plan.id) {
                            report.stalled.push(plan.id.clone());
                        }
                        break;
                    }
                }
                continue;
            }
            if matches!(record.kind.as_str(), "tool_result" | "file_diff" | "diagnostics")
                && record.plan.as_deref() == Some(plan.id.as_str())
            {
                let Some(step) = record.step.clone().and_then(|id| {
                    plan.steps.iter_mut().find(|step| step.id == id)
                }) else {
                    continue;
                };
                if !step.evidence.iter().any(|reference| {
                    reference.session == sess && reference.seq == record.seq
                }) {
                    step.evidence.push(EvidenceRef {
                        session: sess.clone(),
                        seq: record.seq,
                    });
                    report.evidence_reattached += 1;
                    dirty = true;
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
                    && other
                        .fields
                        .get("plan_id")
                        .and_then(|value| value.as_str())
                        == fields.get("result_id").and_then(|value| value.as_str())
            });
            if deleted_after {
                continue;
            }
            match fields.get("op").and_then(|value| value.as_str()) {
                Some("create") => {
                    let Some(result_id) = fields
                        .get("result_id")
                        .and_then(|value| value.as_str())
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
                    let Some(new_id) =
                        fields.get("new_id").and_then(|value| value.as_str())
                    else {
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
    let steps: Vec<NewStep> = serde_json::from_value(
        get("steps")?.clone(),
    )
    .ok()?;
    let budget_limit = get("budget_limit").and_then(|value| value.as_u64()).unwrap_or(0);
    let mut plan = create(goal, constraints, acceptance, steps, budget_limit, &Limits::default()).ok()?;
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

fn rebuild_accepted(
    root: &Path,
    sess: &str,
    seq: u64,
    fields: &serde_json::Map<String, serde_json::Value>,
) -> Option<Plan> {
    let draft: PlanDraftArgs = serde_json::from_value(fields.get("draft")?.clone()).ok()?;
    let mut fresh = draft.build(u64::MAX, &Limits::default()).ok()?;
    fresh.id = fields.get("new_id")?.as_str()?.to_string();
    fresh.created = fields.get("new_created")?.as_str()?.to_string();
    fresh.sessions = fields
        .get("new_sessions")?
        .as_array()?
        .iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect();
    if let Some(abandoned) = fields.get("abandoned").and_then(|value| value.as_str())
        && let Some(mut old) = read_plan_file(root, abandoned)
        && old.status == PlanStatus::Active
    {
        old.status = PlanStatus::Abandoned;
        old.revision = old.revision.saturating_add(1);
        old.applied_event = Some(scoped(sess, seq));
        store(root, &old).ok()?;
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
            verify_acceptance(plan, index, evidence, false).map(|_| true)
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
        Some(
            "start" | "finish" | "block" | "unblock" | "cancel" | "add" | "split" | "complete",
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
    let tmp = dir.join(format!("{}.json.tmp", plan.id));
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

/// At most one active plan per project (§2.1.1).
pub fn open_active(root: &Path) -> Result<Option<Plan>> {
    open_active_for_session(root, None)
}

/// Open the active plan for a specific session, or the most recent active plan.
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

// ---------------------------------------------------------------- operations

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewStep {
    pub title: String,
    #[serde(default)]
    pub kind: Option<StepKind>,
    #[serde(default)]
    pub refs: Vec<StepRef>,
}

/// One operation per call (§2.1.3). `Serialize` is for the journal intent
/// record: every accepted op is journaled with its full args before the
/// plan file is stored, so a crash between the two heals by replay (§2.1.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Create {
        goal: String,
        #[serde(default)]
        constraints: Vec<String>,
        #[serde(default)]
        acceptance: Vec<String>,
        #[serde(default)]
        steps: Vec<NewStep>,
    },
    Start {
        id: String,
        #[serde(default)]
        confirm: Option<bool>,
    },
    Finish {
        id: String,
        summary: String,
        /// Deprecated input retained for wire compatibility; the host ignores it.
        #[serde(default)]
        evidence: Vec<u64>,
    },
    Block {
        id: String,
        reason: String,
    },
    Unblock {
        id: String,
    },
    Cancel {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        reason: String,
    },
    Add {
        #[serde(default)]
        after: Option<String>,
        title: String,
        #[serde(default)]
        kind: Option<StepKind>,
        #[serde(default)]
        refs: Vec<StepRef>,
    },
    Split {
        id: String,
        into: Vec<NewStep>,
    },
    Verify {
        acceptance: usize,
        /// Deprecated input retained for wire compatibility; the host ignores it.
        #[serde(default)]
        evidence: Vec<u64>,
    },
    Complete,
    Show,
}

#[derive(Debug, Clone)]
pub struct Rejection {
    pub code: &'static str,
    pub reason: String,
    pub hint: String,
}

impl Rejection {
    fn new(code: &'static str, reason: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
            hint: hint.into(),
        }
    }
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // Created(Plan) is intentionally large; boxing would change call sites
pub enum Applied {
    /// `create` result — the caller must persist the plan.
    #[allow(dead_code)]
    Created(Plan),
    Updated {
        message: String,
    },
    Shown {
        text: String,
    },
    Completed,
}

/// Arguments of a full-plan proposal: the same shape as `Op::Create`
/// without the active-plan guard. The host validates a draft with
/// `create` before the user ever sees it, and stores the rebuilt plan on
/// accept — the agent never writes plan state itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanDraftArgs {
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub acceptance: Vec<String>,
    #[serde(default)]
    pub steps: Vec<NewStep>,
}

impl PlanDraftArgs {
    pub fn build(&self, budget_limit: u64, limits: &Limits) -> Result<Plan, Rejection> {
        create(
            self.goal.clone(),
            self.constraints.clone(),
            self.acceptance.clone(),
            self.steps.clone(),
            budget_limit,
            limits,
        )
    }
}

/// Host-only: retire the active plan when the user accepts a replacement
/// proposal. The reason travels in the journal; the file keeps no history
/// of why it was abandoned.
pub fn abandon(plan: &mut Plan) {
    plan.status = PlanStatus::Abandoned;
    plan.revision += 1;
}

/// Host-side validation of a proposed plan draft against the current active plan (§2.1.6).
///
/// If the goal is unchanged (the model is refining/adjusting steps under the same goal),
/// the model is not allowed to silently weaken commitments:
/// 1. Active constraints cannot be dropped.
/// 2. Active acceptance criteria cannot be dropped.
/// 3. Pending steps cannot be silently deleted if work was already attempted on them.
///
/// If the goal is changed (user-initiated goal revision or model-proposed pivot),
/// a new goal is declared and the diff will be explicitly reviewed and accepted by the user.
pub fn validate_proposal_invariants(
    active: Option<&Plan>,
    draft: &PlanDraftArgs,
) -> Result<(), Rejection> {
    let Some(active) = active else {
        return Ok(());
    };

    let same_goal = active.goal.text.trim() == draft.goal.trim();
    if same_goal {
        // 1. Constraints monotonicity under the same goal
        for constraint in &active.constraints {
            let found = draft
                .constraints
                .iter()
                .any(|c| c.trim() == constraint.trim());
            if !found {
                return Err(Rejection::new(
                    "weakened_constraints",
                    format!(
                        "proposal removes active constraint '{constraint}' under the same goal"
                    ),
                    "keep existing constraints or propose a goal revision if the task direction changed",
                ));
            }
        }

        // 2. Acceptance criteria preservation under the same goal
        for acc in &active.acceptance {
            let found = draft.acceptance.iter().any(|a| a.trim() == acc.text.trim());
            if !found {
                return Err(Rejection::new(
                    "dropped_acceptance",
                    format!(
                        "proposal drops active acceptance item '{}' under the same goal",
                        acc.text
                    ),
                    "keep existing acceptance criteria; manual acceptance can only be waived by user",
                ));
            }
        }

        // 3. Pending steps preservation: cannot drop steps that had attempts or evidence
        for step in &active.steps {
            if !step.evidence.is_empty() {
                // If a step has host evidence, its work or title should remain tracked
                let title_retained = draft
                    .steps
                    .iter()
                    .any(|s| s.title.trim() == step.title.trim());
                if !title_retained {
                    return Err(Rejection::new(
                        "dropped_evidenced_step",
                        format!(
                            "proposal drops step '{}' which already has recorded evidence",
                            step.title
                        ),
                        "steps with recorded evidence cannot be removed without trace; keep them in the proposal",
                    ));
                }
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_steps: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_steps: MAX_STEPS_DEFAULT,
        }
    }
}

/// Build a new plan. Rejects an empty goal, zero steps and over-long plans.
pub fn create(
    goal: String,
    constraints: Vec<String>,
    acceptance: Vec<String>,
    steps: Vec<NewStep>,
    budget_limit: u64,
    limits: &Limits,
) -> Result<Plan, Rejection> {
    if goal.trim().is_empty() {
        return Err(Rejection::new(
            "empty_goal",
            "a plan needs a goal",
            "state what must be true when the work is done",
        ));
    }
    if steps.is_empty() {
        return Err(Rejection::new(
            "no_steps",
            "a plan needs at least one step",
            "break the work into 3-12 steps",
        ));
    }
    if steps.len() > limits.max_steps {
        return Err(Rejection::new(
            "too_many_steps",
            format!(
                "{} steps exceeds the limit of {}",
                steps.len(),
                limits.max_steps
            ),
            "merge steps, or ask the user to raise it with /plan limit N",
        ));
    }

    let ts = now();
    let plan = Plan {
        version: 1,
        id: new_id(),
        status: PlanStatus::Active,
        created: ts.clone(),
        sessions: Vec::new(),
        applied_event: None,
        goal: Goal {
            text: goal,
            source: "user".to_string(),
            created: ts,
            history: Vec::new(),
        },
        constraints,
        acceptance: acceptance
            .into_iter()
            .map(|text| Acceptance {
                text,
                status: AcceptanceStatus::Pending,
                evidence: Vec::new(),
                validation: Validation::default(),
                by: None,
                reason: None,
            })
            .collect(),
        steps: steps
            .into_iter()
            .enumerate()
            .map(|(i, s)| Step {
                id: (i + 1).to_string(),
                title: s.title,
                kind: s.kind.unwrap_or(StepKind::Change),
                status: StepStatus::Pending,
                started: None,
                finished: None,
                summary: None,
                reason: None,
                evidence: Vec::new(),
                refs: s.refs,
                validation: Validation::default(),
                step_epoch: 0,
                stale_goal: None,
            })
            .collect(),
        folded: Vec::new(),
        budget: Budget {
            tokens: 0,
            limit: budget_limit,
        },
        revision: 0,
        rejections_in_a_row: 0,
    };
    Ok(plan)
}

/// Apply one operation. Every rule from §2.1.4 except the evidence rule and
/// refs (deferred to F3 / I4).
pub fn apply(
    plan: &mut Plan,
    op: Op,
    limits: &Limits,
    current_step: Option<&str>,
) -> Result<Applied, Rejection> {
    match op {
        Op::Create { .. } => reject(
            plan,
            "plan_exists",
            format!("an active plan already exists: {}", plan.id),
            "use /plan to continue, complete or abandon it first",
        ),
        Op::Show => {
            plan.rejections_in_a_row = 0;
            Ok(Applied::Shown { text: render(plan) })
        }
        Op::Start { id, confirm } => start(plan, &id, confirm, current_step),
        Op::Finish {
            id,
            summary,
            evidence,
        } => finish(plan, &id, summary, !evidence.is_empty()),
        Op::Block { id, reason } => block(plan, &id, reason),
        Op::Unblock { id } => unblock(plan, &id),
        Op::Cancel { id, reason } => cancel(plan, id.as_deref(), reason),
        Op::Add {
            after,
            title,
            kind,
            refs,
        } => add(plan, after.as_deref(), title, kind, refs, limits),
        Op::Split { id, into } => split(plan, &id, into, limits),
        // The host prepares the evidence (and runs `cmd:` items) before
        // applying a verify; reaching it through `apply` alone means there is
        // none, which `verify_acceptance` rejects for anything but `cmd:`.
        Op::Verify {
            acceptance,
            evidence,
        } => verify_acceptance(plan, acceptance, Vec::new(), !evidence.is_empty()),
        Op::Complete => complete(plan),
    }
}

fn reject(
    plan: &mut Plan,
    code: &'static str,
    reason: impl Into<String>,
    hint: impl Into<String>,
) -> Result<Applied, Rejection> {
    plan.rejections_in_a_row += 1;
    Err(Rejection::new(code, reason, hint))
}

fn accept(plan: &mut Plan, message: impl Into<String>) -> Result<Applied, Rejection> {
    plan.revision += 1;
    plan.rejections_in_a_row = 0;
    Ok(Applied::Updated {
        message: message.into(),
    })
}

/// Gate fields read without holding a borrow, so a rejection can still bump
/// `rejections_in_a_row`.
fn step_status(plan: &Plan, id: &str) -> Option<(StepStatus, bool)> {
    plan.step(id)
        .map(|s| (s.status, s.stale_goal == Some(true)))
}

fn unknown_step(plan: &mut Plan, id: &str) -> Result<Applied, Rejection> {
    reject(
        plan,
        "unknown_step",
        format!("no step {id} in this plan"),
        "call plan show to see the current steps",
    )
}

fn start(
    plan: &mut Plan,
    id: &str,
    confirm: Option<bool>,
    current_step: Option<&str>,
) -> Result<Applied, Rejection> {
    let Some((status, stale)) = step_status(plan, id) else {
        return unknown_step(plan, id);
    };
    if !matches!(status, StepStatus::Pending | StepStatus::Reopened) {
        return reject(
            plan,
            "step_not_pending",
            format!("step {id} is {}", status.as_str()),
            "only a pending or reopened step can be started",
        );
    }
    // One step at a time (§2.2.3): the caller's session must not hold another
    // step, and the plan must not have one in progress either (a second
    // InProgress step would make dispatch attribution ambiguous).
    if let Some(other) = current_step.filter(|current| *current != id) {
        return reject(
            plan,
            "step_busy",
            format!("step {other} is already the current step of this session"),
            format!("finish, block or cancel step {other} first — or continue it instead of starting step {id}"),
        );
    }
    if let Some(other) = plan.steps.iter().find_map(|step| {
        (step.id != id && step.status == StepStatus::InProgress).then(|| step.id.clone())
    }) {
        return reject(
            plan,
            "step_busy",
            format!("step {other} is already in progress"),
            format!("only one step runs at a time; finish, block or cancel step {other} first"),
        );
    }
    if stale && confirm != Some(true) {
        return reject(
            plan,
            "stale_goal",
            format!("step {id} predates the current goal"),
            "re-read it against the new goal, then pass confirm: true",
        );
    }
    let step = plan.step_mut(id).expect("checked above");
    step.status = StepStatus::InProgress;
    step.started = Some(now());
    step.stale_goal = None;
    accept(plan, format!("step {id} in progress"))
}

fn finish(
    plan: &mut Plan,
    id: &str,
    summary: String,
    supplied_evidence: bool,
) -> Result<Applied, Rejection> {
    let Some((status, _)) = step_status(plan, id) else {
        return unknown_step(plan, id);
    };
    if status != StepStatus::InProgress {
        return reject(
            plan,
            "step_not_in_progress",
            format!("step {id} is {}", status.as_str()),
            "start the step before finishing it",
        );
    }
    if summary.trim().is_empty() {
        return reject(
            plan,
            "empty_summary",
            format!("step {id} needs a summary of what was done"),
            "one line: what changed and where",
        );
    }
    // Evidence presence and kind are validated by the host tool dispatcher,
    // after host journal records have been attached to this step.
    let step = plan.step_mut(id).expect("checked above");
    step.status = StepStatus::Done;
    step.finished = Some(now());
    step.summary = Some(summary);
    let message = if supplied_evidence {
        format!("step {id} done (model evidence ignored; host evidence used)")
    } else {
        format!("step {id} done")
    };
    accept(plan, message)
}

fn block(plan: &mut Plan, id: &str, reason: String) -> Result<Applied, Rejection> {
    let Some((status, _)) = step_status(plan, id) else {
        return unknown_step(plan, id);
    };
    if status != StepStatus::InProgress {
        return reject(
            plan,
            "step_not_in_progress",
            format!("step {id} is {}", status.as_str()),
            "only an in-progress step can be blocked",
        );
    }
    if reason.trim().is_empty() {
        return reject(
            plan,
            "empty_reason",
            format!("blocking step {id} needs a reason"),
            "say what it is waiting for",
        );
    }
    let step = plan.step_mut(id).expect("checked above");
    step.status = StepStatus::Blocked;
    step.reason = Some(reason);
    accept(plan, format!("step {id} blocked"))
}

fn unblock(plan: &mut Plan, id: &str) -> Result<Applied, Rejection> {
    let Some((status, _)) = step_status(plan, id) else {
        return unknown_step(plan, id);
    };
    if status != StepStatus::Blocked {
        return reject(
            plan,
            "step_not_blocked",
            format!("step {id} is {}", status.as_str()),
            "only a blocked step can be unblocked",
        );
    }
    let step = plan.step_mut(id).expect("checked above");
    step.status = StepStatus::Pending;
    step.reason = None;
    accept(plan, format!("step {id} unblocked"))
}

fn cancel(plan: &mut Plan, id: Option<&str>, reason: String) -> Result<Applied, Rejection> {
    let Some(id) = id else {
        plan.status = PlanStatus::Abandoned;
        plan.revision += 1;
        return accept(plan, format!("plan {} cancelled", plan.id));
    };
    if id == plan.id {
        plan.status = PlanStatus::Abandoned;
        plan.revision += 1;
        return accept(plan, format!("plan {} cancelled", plan.id));
    }
    let Some((status, _)) = step_status(plan, id) else {
        return unknown_step(plan, id);
    };
    if status == StepStatus::Done {
        return reject(
            plan,
            "step_done",
            format!("step {id} is already done"),
            "cancel work that will not happen, not finished work",
        );
    }
    let reason_str = if reason.trim().is_empty() {
        "cancelled".to_string()
    } else {
        reason
    };
    let step = plan.step_mut(id).expect("checked above");
    step.status = StepStatus::Cancelled;
    step.reason = Some(reason_str);
    accept(plan, format!("step {id} cancelled"))
}

fn add(
    plan: &mut Plan,
    after: Option<&str>,
    title: String,
    kind: Option<StepKind>,
    refs: Vec<StepRef>,
    limits: &Limits,
) -> Result<Applied, Rejection> {
    if title.trim().is_empty() {
        return reject(
            plan,
            "empty_title",
            "a new step needs a title",
            "one line describing the work",
        );
    }
    if plan.steps.len() + 1 > limits.max_steps {
        return reject(
            plan,
            "too_many_steps",
            format!(
                "adding a step would exceed the limit of {}",
                limits.max_steps
            ),
            "merge or cancel steps first, or /plan limit N",
        );
    }
    let index = match after {
        None => plan.steps.len(),
        Some(after_id) => {
            let found = plan.steps.iter().position(|s| s.id == after_id);
            match found {
                Some(i) => i + 1,
                None => {
                    return reject(
                        plan,
                        "unknown_step",
                        format!("no step {after_id} to add after"),
                        "call plan show to see the current steps",
                    );
                }
            }
        }
    };
    let step = Step {
        id: next_id(plan),
        title,
        kind: kind.unwrap_or(StepKind::Change),
        status: StepStatus::Pending,
        started: None,
        finished: None,
        summary: None,
        reason: None,
        evidence: Vec::new(),
        refs,
        validation: Validation::default(),
        step_epoch: 0,
        stale_goal: None,
    };
    let id = step.id.clone();
    plan.steps.insert(index, step);
    accept(plan, format!("step {id} added"))
}

fn split(
    plan: &mut Plan,
    id: &str,
    into: Vec<NewStep>,
    limits: &Limits,
) -> Result<Applied, Rejection> {
    if into.is_empty() {
        return reject(
            plan,
            "empty_split",
            format!("splitting step {id} needs at least two parts"),
            "give the step a smaller shape, or cancel it with a reason",
        );
    }
    let found = plan.steps.iter().position(|s| s.id == id);
    let index = match found {
        Some(i) => i,
        None => {
            return reject(
                plan,
                "unknown_step",
                format!("no step {id} to split"),
                "call plan show to see the current steps",
            );
        }
    };
    let resulting = plan.steps.len() - 1 + into.len();
    if resulting > limits.max_steps {
        return reject(
            plan,
            "too_many_steps",
            format!(
                "splitting would give {resulting} steps, over the limit of {max}",
                max = limits.max_steps
            ),
            "cancel or merge steps first, or /plan limit N",
        );
    }
    let suffixes = ["a", "b", "c", "d", "e", "f", "g", "h"];
    if into.len() > suffixes.len() {
        return reject(
            plan,
            "too_many_parts",
            format!("splitting into {} parts is not supported", into.len()),
            "split in two stages",
        );
    }
    let replacements: Vec<Step> = into
        .into_iter()
        .enumerate()
        .map(|(i, s)| Step {
            id: format!("{id}{}", suffixes[i]),
            title: s.title,
            kind: s.kind.unwrap_or(StepKind::Change),
            status: StepStatus::Pending,
            started: None,
            finished: None,
            summary: None,
            reason: None,
            evidence: Vec::new(),
            refs: s.refs,
            validation: Validation::default(),
            step_epoch: 0,
            stale_goal: None,
        })
        .collect();
    let ids: Vec<String> = replacements.iter().map(|s| s.id.clone()).collect();
    plan.steps.splice(index..=index, replacements);
    accept(plan, format!("step {id} split into {}", ids.join(", ")))
}

/// Mark an acceptance item verified on the host's terms.
///
/// `evidence` is what the host is prepared to stand behind for *this* item:
/// the scoped journal references for a `Text` item, and empty for a `Command`
/// item, whose verification is the host having just run the command (and
/// running it again at `complete`).
///
/// This used to take no evidence at all and dig out `steps.iter().find(kind ==
/// Verify && !evidence.is_empty())` — the first verify step with anything
/// attached, regardless of which acceptance item was being verified. One
/// successful command let every acceptance item pass in turn, reusing the same
/// record, so "complete requires every acceptance criterion verified" meant
/// "something succeeded once".
pub fn verify_acceptance(
    plan: &mut Plan,
    index: usize,
    evidence: Vec<EvidenceRef>,
    supplied_evidence: bool,
) -> Result<Applied, Rejection> {
    if index >= plan.acceptance.len() {
        return reject(
            plan,
            "unknown_acceptance",
            format!("no acceptance item {index}"),
            "call plan show to see the acceptance list",
        );
    }
    match plan.acceptance[index].kind() {
        AcceptanceKind::Manual(text) => {
            return reject(
                plan,
                "manual_acceptance",
                format!("acceptance {index} is manual: {text}"),
                "no command settles this one; ask the user to waive it with \
                 /plan waive",
            );
        }
        AcceptanceKind::Command(_) => {
            // the host ran it before calling this; nothing to point at, and
            // `complete` runs it again rather than trusting an old record
        }
        AcceptanceKind::Text(_) => {
            if evidence.is_empty() {
                return reject(
                    plan,
                    "no_evidence",
                    format!("acceptance {index} has no host evidence of its own"),
                    "close a verify step whose evidence is not already \
                     spent on another acceptance item, or prefix the item \
                     with cmd: so the host can run it",
                );
            }
            // Two items cannot lean on the same record: that is the reuse
            // this function exists to prevent.
            let spent: Vec<&EvidenceRef> = plan
                .acceptance
                .iter()
                .enumerate()
                .filter(|(other, item)| {
                    *other != index && item.status == AcceptanceStatus::Passed
                })
                .flat_map(|(_, item)| item.evidence.iter())
                .collect();
            if let Some(clash) = evidence.iter().find(|reference| {
                spent
                    .iter()
                    .any(|used| used.session == reference.session && used.seq == reference.seq)
            }) {
                return reject(
                    plan,
                    "evidence_spent",
                    format!(
                        "journal record {}:{} already verifies another acceptance item",
                        clash.session, clash.seq
                    ),
                    "run the check for this item so it has evidence of its own",
                );
            }
        }
    }
    let item = &mut plan.acceptance[index];
    item.status = AcceptanceStatus::Passed;
    item.evidence = evidence;
    let message = if supplied_evidence {
        format!("acceptance {index} verified (model evidence ignored; host evidence used)")
    } else {
        format!("acceptance {index} verified")
    };
    accept(plan, message)
}

fn complete(plan: &mut Plan) -> Result<Applied, Rejection> {
    let pending: Vec<String> = plan
        .steps
        .iter()
        .filter(|s| {
            matches!(
                s.status,
                StepStatus::Pending
                    | StepStatus::InProgress
                    | StepStatus::Blocked
                    | StepStatus::Reopened
            )
        })
        .map(|s| s.id.clone())
        .collect();
    if !pending.is_empty() {
        return reject(
            plan,
            "steps_open",
            format!(            "steps still open: {}", pending.join(", ")),
            "finish, unblock or cancel them first",
        );
    }
    let unverified: Vec<usize> = plan
        .acceptance
        .iter()
        .enumerate()
        .filter(|(_, a)| a.status == AcceptanceStatus::Pending)
        .map(|(i, _)| i)
        .collect();
    if !unverified.is_empty() {
        return reject(
            plan,
            "acceptance_pending",
            format!(
                "acceptance items still pending: {}",
                unverified
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            "verify them, or have the user waive them with /plan waive. Pending acceptance items without cmd: prefix require user waiver (/plan waive <index>) or conversion to verify steps.",
        );
    }
    plan.status = PlanStatus::Completed;
    plan.revision += 1;
    plan.rejections_in_a_row = 0;
    Ok(Applied::Completed)
}

fn next_id(plan: &Plan) -> String {
    let max = plan
        .steps
        .iter()
        .filter_map(|s| s.id.parse::<usize>().ok())
        .max()
        .unwrap_or(0);
    (max + 1).to_string()
}

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
pub fn waive(plan: &mut Plan, index: usize, reason: &str) -> Result<(), Rejection> {
    if index >= plan.acceptance.len() {
        return Err(Rejection::new(
            "unknown_acceptance",
            format!("no acceptance item {index}"),
            "call /plan to see the acceptance list",
        ));
    }
    let item = &mut plan.acceptance[index];
    item.status = AcceptanceStatus::Waived;
    item.by = Some("user".to_string());
    item.reason = Some(reason.to_string());
    plan.revision += 1;
    Ok(())
}

// ---------------------------------------------------------------- rendering

/// The plan document shown by `/plan` (§2.1.7).
pub fn render(plan: &Plan) -> String {
    let c = plan.counts();
    let mut out = String::new();
    out.push_str(&format!(
        "plan {} · {}\n",
        plan.id,
        status_word(plan.status)
    ));
    out.push_str(&format!("goal: {}\n", plan.goal.text));
    if !plan.constraints.is_empty() {
        out.push_str(&format!("constraints: {}\n", plan.constraints.join(" · ")));
    }
    if !plan.acceptance.is_empty() {
        out.push_str("acceptance:\n");
        for (i, a) in plan.acceptance.iter().enumerate() {
            out.push_str(&format!("  [{}] {} {}\n", i, a.status.as_str(), a.text));
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
        let mut line = format!("  {} {} ({}) {}", marker, s.id, s.kind.as_str(), s.title);
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

fn status_word(status: PlanStatus) -> &'static str {
    match status {
        PlanStatus::Active => "active",
        PlanStatus::Completed => "completed",
        PlanStatus::Abandoned => "abandoned",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_plan() -> Plan {
        create(
            "persist the plan on disk".to_string(),
            vec!["no new dependencies".to_string()],
            vec!["cmd: cargo test".to_string()],
            vec![
                NewStep {
                    title: "add the model".to_string(),
                    kind: None,
                    refs: Vec::new(),
                },
                NewStep {
                    title: "add the validator".to_string(),
                    kind: None,
                    refs: Vec::new(),
                },
            ],
            20000,
            &Limits::default(),
        )
        .expect("plan creates")
    }

    #[test]
    fn ulid_shape() {
        let id = new_id();
        assert_eq!(id.len(), 26, "ULID is 26 chars: {id}");
        assert!(
            id.chars()
                .all(|c| "0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(c)),
            "Crockford base32 only: {id}"
        );
    }

    #[test]
    fn undo_reopens_done_step_and_clears_completion_metadata() {
        let mut plan = new_plan();
        apply(
            &mut plan,
            Op::Start {
                id: "1".into(),
                confirm: None,
            },
            &Limits::default(),
            None,
        )
        .unwrap();
        plan.step_mut("1").unwrap().evidence.push(EvidenceRef {
            session: "test".into(),
            seq: 42,
        });
        apply(
            &mut plan,
            Op::Finish {
                id: "1".into(),
                summary: "model added".into(),
                evidence: vec![42],
            },
            &Limits::default(),
            None,
        )
        .unwrap();
        let revision = plan.revision;

        reopen_for_undo(&mut plan, "1", "reopened by undo").unwrap();

        let step = plan.step("1").unwrap();
        assert_eq!(step.status, StepStatus::Reopened);
        assert_eq!(step.finished, None);
        assert_eq!(step.summary, None);
        assert_eq!(step.reason.as_deref(), Some("reopened by undo"));
        assert!(step.evidence.is_empty());
        assert_eq!(plan.revision, revision + 1);
        assert!(
            apply(
                &mut plan,
                Op::Start {
                    id: "1".into(),
                    confirm: None,
                },
                &Limits::default(),
                None,
            )
            .is_ok()
        );
    }

    #[test]
    fn undo_leaves_unrelated_steps_and_non_done_steps_unchanged() {
        let mut plan = new_plan();
        plan.steps[0].status = StepStatus::Done;
        plan.steps[0].finished = Some("finished".into());
        plan.steps[0].summary = Some("kept for undo test".into());
        plan.steps[1].status = StepStatus::InProgress;
        plan.steps[1].reason = Some("still working".into());
        let revision = plan.revision;

        reopen_for_undo(&mut plan, "1", "reopened by undo").unwrap();
        reopen_for_undo(&mut plan, "2", "must remain in progress").unwrap();

        assert_eq!(plan.step("1").unwrap().status, StepStatus::Reopened);
        assert_eq!(plan.step("2").unwrap().status, StepStatus::InProgress);
        assert_eq!(
            plan.step("2").unwrap().reason.as_deref(),
            Some("still working")
        );
        assert_eq!(plan.revision, revision + 1);
    }

    #[test]
    fn undo_rejects_unknown_step() {
        let mut plan = new_plan();
        let error = reopen_for_undo(&mut plan, "missing", "reopened by undo").unwrap_err();
        assert!(error.to_string().contains("unknown plan step missing"));
    }

    #[test]
    fn start_then_finish() {
        let mut plan = new_plan();
        assert!(matches!(
            apply(
                &mut plan,
                Op::Start {
                    id: "1".into(),
                    confirm: None
                },
                &Limits::default(),
                None,
            ),
            Ok(Applied::Updated { .. })
        ));
        assert!(matches!(
            apply(
                &mut plan,
                Op::Finish {
                    id: "1".into(),
                    summary: "model added".into(),
                    evidence: vec![1]
                },
                &Limits::default(),
                None,
            ),
            Ok(Applied::Updated { .. })
        ));
        assert_eq!(plan.step("1").unwrap().status, StepStatus::Done);
    }

    #[test]
    fn finish_requires_in_progress() {
        let mut plan = new_plan();
        let err = apply(
            &mut plan,
            Op::Finish {
                id: "2".into(),
                summary: "x".into(),
                evidence: vec![],
            },
            &Limits::default(),
            None,
        )
        .unwrap_err();
        assert_eq!(err.code, "step_not_in_progress");
        assert_eq!(plan.rejections_in_a_row, 1);
    }

    #[test]
    fn complete_requires_all_steps_closed() {
        let mut plan = new_plan();
        let err = apply(&mut plan, Op::Complete, &Limits::default(), None).unwrap_err();
        assert_eq!(err.code, "steps_open");
    }

    #[test]
    fn complete_requires_acceptance() {
        let mut plan = new_plan();
        for id in ["1", "2"] {
            apply(
                &mut plan,
                Op::Start {
                    id: id.into(),
                    confirm: None,
                },
                &Limits::default(),
                None,
            )
            .unwrap();
            apply(
                &mut plan,
                Op::Finish {
                    id: id.into(),
                    summary: "done".into(),
                    evidence: vec![1],
                },
                &Limits::default(),
                None,
            )
            .unwrap();
        }
        let err = apply(&mut plan, Op::Complete, &Limits::default(), None).unwrap_err();
        assert_eq!(err.code, "acceptance_pending");
        assert!(
            err.hint.contains("Pending acceptance items without cmd: prefix require user waiver (/plan waive <index>) or conversion to verify steps."),
            "{}",
            err.hint
        );
        waive(&mut plan, 0, "manual check").unwrap();
        assert!(matches!(
            apply(&mut plan, Op::Complete, &Limits::default(), None),
            Ok(Applied::Completed)
        ));
        assert_eq!(plan.status, PlanStatus::Completed);
    }

    #[test]
    fn goal_revision_marks_pending_steps_stale() {
        let mut plan = new_plan();
        apply(
            &mut plan,
            Op::Start {
                id: "1".into(),
                confirm: None,
            },
            &Limits::default(),
            None,
        )
        .unwrap();
        set_goal(&mut plan, "a different goal".into(), "user", None);
        assert_eq!(plan.goal.text, "a different goal");
        assert_eq!(plan.goal.history.len(), 1);
        assert_eq!(plan.step("2").unwrap().stale_goal, Some(true));
        // step 1 is in progress, so it is not marked stale
        assert_eq!(plan.step("1").unwrap().stale_goal, None);
    }

    #[test]
    fn stale_step_needs_confirm() {
        let mut plan = new_plan();
        set_goal(&mut plan, "a different goal".into(), "user", None);
        let err = apply(
            &mut plan,
            Op::Start {
                id: "1".into(),
                confirm: None,
            },
            &Limits::default(),
            None,
        )
        .unwrap_err();
        assert_eq!(err.code, "stale_goal");
        assert!(
            apply(
                &mut plan,
                Op::Start {
                    id: "1".into(),
                    confirm: Some(true)
                },
                &Limits::default(),
                None,
            )
            .is_ok()
        );
    }

    #[test]
    fn add_and_split_respect_the_limit() {
        let mut plan = new_plan();
        let limits = Limits { max_steps: 3 };
        apply(
            &mut plan,
            Op::Add {
                after: Some("1".into()),
                title: "extra".into(),
                kind: None,
                refs: Vec::new(),
            },
            &limits,
            None,
        )
        .unwrap();
        assert_eq!(plan.steps.len(), 3);
        let err = apply(
            &mut plan,
            Op::Add {
                after: None,
                title: "one too many".into(),
                kind: None,
                refs: Vec::new(),
            },
            &limits,
            None,
        )
        .unwrap_err();
        assert_eq!(err.code, "too_many_steps");
    }

    #[test]
    fn store_and_reopen_round_trip() {
        let dir = std::env::temp_dir().join(format!("sqwai-plan-{}", new_id()));
        let plan = new_plan();
        let id = plan.id.clone();
        store(&dir, &plan).unwrap();
        let loaded = open(&dir, &id).unwrap();
        assert_eq!(loaded.goal.text, plan.goal.text);
        assert_eq!(loaded.steps.len(), 2);
        assert_eq!(open_active(&dir).unwrap().map(|p| p.id), Some(id.clone()));
        assert_eq!(list(&dir).len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_plan_is_moved_aside() {
        let dir = std::env::temp_dir().join(format!("sqwai-plan-{}", new_id()));
        let plans = plans_dir(&dir);
        std::fs::create_dir_all(&plans).unwrap();
        let id = "01JNOTAPLAN";
        std::fs::write(plans.join(format!("{id}.json")), "{ not json").unwrap();
        assert!(open(&dir, id).is_err());
        assert!(plans.join("corrupt").join(format!("{id}.json")).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn proposal_invariants_reject_dropping_constraints_under_same_goal() {
        let active = new_plan(); // has constraint: "no new dependencies", acceptance: "cmd: cargo test"
        let mut draft = PlanDraftArgs {
            goal: active.goal.text.clone(),
            constraints: vec![], // dropped!
            acceptance: vec!["cmd: cargo test".into()],
            steps: vec![NewStep {
                title: "step 1".into(),
                kind: None,
                refs: vec![],
            }],
        };

        // 1. Weakened constraints rejected
        let res = validate_proposal_invariants(Some(&active), &draft);
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().code, "weakened_constraints");

        // 2. Preserved constraints accepted
        draft.constraints = active.constraints.clone();
        assert!(validate_proposal_invariants(Some(&active), &draft).is_ok());

        // 3. Dropped acceptance rejected
        draft.acceptance = vec![];
        let res = validate_proposal_invariants(Some(&active), &draft);
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().code, "dropped_acceptance");

        // 4. Changing goal allows new constraints and acceptance
        draft.goal = "A completely different goal".into();
        draft.constraints = vec!["new constraint".into()];
        draft.acceptance = vec!["new acceptance".into()];
        assert!(validate_proposal_invariants(Some(&active), &draft).is_ok());
    }

    #[test]
    fn complete_rejects_reopened_steps() {
        let mut plan = new_plan();
        plan.steps[0].status = StepStatus::Done;
        plan.steps[1].status = StepStatus::Reopened;
        plan.acceptance[0].status = AcceptanceStatus::Passed;
        let err = complete(&mut plan).unwrap_err();
        assert_eq!(err.code, "steps_open");
        assert!(err.reason.contains('2'), "reason: {}", err.reason);
    }

    #[test]
    fn replay_heals_crash_between_journal_and_store() {
        let dir = std::env::temp_dir().join(format!("sqwai-plan-replay-{}", new_id()));
        let mut plan = new_plan();
        plan.applied_event = Some("crash:0".to_string());
        store(&dir, &plan).unwrap();
        // The crash: intent journaled, plan file never caught up.
        let mut journal =
            crate::agent::journal::Journal::open(&dir, "crash").unwrap();
        let seq = journal
            .append(
                "plan",
                serde_json::json!({
                    "op": "start", "id": "1",
                    "plan_id": plan.id, "by": "model", "ok": true,
                }),
            )
            .unwrap();
        assert_eq!(seq, 1);

        let report = replay(&dir).unwrap();
        assert_eq!(report.ops_applied, 1);
        assert_eq!(report.plans_healed, vec![plan.id.clone()]);
        let healed = open(&dir, &plan.id).unwrap();
        assert_eq!(
            healed.step("1").unwrap().status,
            StepStatus::InProgress
        );
        assert_eq!(healed.applied_event.as_deref(), Some("crash:1"));

        // Idempotent: a second run changes nothing and stores nothing.
        let again = replay(&dir).unwrap();
        assert_eq!(again.ops_applied, 0);
        assert!(again.plans_healed.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn replay_rebuilds_orphan_create_and_respects_delete() {
        let dir = std::env::temp_dir().join(format!("sqwai-plan-orphan-{}", new_id()));
        let mut journal =
            crate::agent::journal::Journal::open(&dir, "orphan").unwrap();
        journal
            .append(
                "plan",
                serde_json::json!({
                    "op": "create", "by": "model", "ok": true,
                    "plan_id": "01J000ORPHAN00000000000001",
                    "goal": "orphaned goal",
                    "constraints": [],
                    "acceptance": ["cmd: true"],
                    "steps": [{"title": "s1", "kind": "change", "refs": []}],
                    "budget_limit": 20000,
                    "result_id": "01J000ORPHAN00000000000001",
                    "result_created": "2026-01-01T00:00:00+00:00",
                    "result_sessions": ["orphan"],
                }),
            )
            .unwrap();

        let report = replay(&dir).unwrap();
        assert_eq!(report.orphans_rebuilt, vec!["01J000ORPHAN00000000000001"]);
        let rebuilt = open(&dir, "01J000ORPHAN00000000000001").unwrap();
        assert_eq!(rebuilt.goal.text, "orphaned goal");
        assert_eq!(rebuilt.applied_event.as_deref(), Some("orphan:1"));

        // Deliberate absence: a later plan_deleted means do not resurrect.
        journal
            .append(
                "plan_deleted",
                serde_json::json!({"plan_id": "01J000ORPHAN00000000000001"}),
            )
            .unwrap();
        std::fs::remove_file(
            plans_dir(&dir).join("01J000ORPHAN00000000000001.json"),
        )
        .unwrap();
        let report = replay(&dir).unwrap();
        assert!(report.orphans_rebuilt.is_empty());
        assert!(open(&dir, "01J000ORPHAN00000000000001").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn replay_skips_rejected_intents() {
        let dir = std::env::temp_dir().join(format!("sqwai-plan-repskip-{}", new_id()));
        let mut plan = new_plan();
        plan.applied_event = Some("rs:0".to_string());
        store(&dir, &plan).unwrap();
        let mut journal = crate::agent::journal::Journal::open(&dir, "rs").unwrap();
        journal
            .append(
                "plan",
                serde_json::json!({
                    "op": "start", "id": "1",
                    "plan_id": plan.id, "by": "model", "ok": false,
                }),
            )
            .unwrap();

        let report = replay(&dir).unwrap();
        assert_eq!(report.ops_applied, 0);
        assert_eq!(open(&dir, &plan.id).unwrap().step("1").unwrap().status, StepStatus::Pending);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn replay_applies_verify_with_recorded_evidence() {
        let dir = std::env::temp_dir().join(format!("sqwai-plan-repver-{}", new_id()));
        let mut plan = create(
            "goal".to_string(),
            Vec::new(),
            vec!["check the docs".to_string()],
            vec![NewStep {
                title: "verify docs".into(),
                kind: Some(StepKind::Verify),
                refs: Vec::new(),
            }],
            1000,
            &Limits::default(),
        )
        .unwrap();
        plan.applied_event = Some("rv:7".to_string());
        store(&dir, &plan).unwrap();
        let mut journal = crate::agent::journal::Journal::open(&dir, "rv").unwrap();
        // Seven filler records so the intent lands on seq 8, past the cursor.
        for _ in 0..7 {
            journal.append("note", serde_json::json!({"note": "x", "kind": "decision"})).unwrap();
        }
        journal
            .append(
                "plan",
                serde_json::json!({
                    "op": "verify", "acceptance": 0,
                    "evidence_refs": [{"session": "rv", "seq": 3}],
                    "plan_id": plan.id, "by": "model", "ok": true,
                }),
            )
            .unwrap();

        let report = replay(&dir).unwrap();
        assert_eq!(report.ops_applied, 1);
        assert_eq!(
            open(&dir, &plan.id).unwrap().acceptance[0].status,
            AcceptanceStatus::Passed
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn complete_rejects_blocked_steps() {
        let mut plan = new_plan();
        apply(
            &mut plan,
            Op::Start {
                id: "1".into(),
                confirm: None,
            },
            &Limits::default(),
            None,
        )
        .unwrap();
        apply(
            &mut plan,
            Op::Block {
                id: "1".into(),
                reason: "waiting".into(),
            },
            &Limits::default(),
            None,
        )
        .unwrap();
        let err = apply(&mut plan, Op::Complete, &Limits::default(), None).unwrap_err();
        assert_eq!(err.code, "steps_open");
        assert!(err.reason.contains('1'), "reason: {}", err.reason);
    }

    #[test]
    fn legacy_plan_file_loads_with_phase0_defaults() {
        let dir = std::env::temp_dir().join(format!("sqwai-plan-legacy-{}", new_id()));
        let plans = plans_dir(&dir);
        std::fs::create_dir_all(&plans).unwrap();
        let id = "01JLEGACYPLAN00000000000001";
        std::fs::write(
            plans.join(format!("{id}.json")),
            serde_json::json!({
                "version": 1,
                "id": id,
                "status": "active",
                "created": "2026-01-01T00:00:00+00:00",
                "forked_from": "01JOLDPARENT00000000000000",
                "sessions": ["s1"],
                "goal": {"text": "old goal", "source": "user", "created": "..."},
                "acceptance": [
                    {"text": "cmd: cargo test", "status": "verified", "evidence": [3]},
                ],
                "steps": [
                    {"id": "1", "title": "old step", "status": "done",
                     "evidence": [1, 2],
                     "refs": ["src/session/mod.rs::fn::save", "src/plain.rs"]},
                ],
                "budget": {"tokens": 10, "limit": 20000},
                "revision": 2,
            })
            .to_string(),
        )
        .unwrap();
        let plan = open(&dir, id).unwrap();
        // `verified` stays readable and means passed
        assert_eq!(plan.acceptance[0].status, AcceptanceStatus::Passed);
        // bare seq evidence keeps the legacy empty-session identity
        assert_eq!(
            plan.acceptance[0].evidence,
            vec![EvidenceRef {
                session: String::new(),
                seq: 3
            }]
        );
        // phase-0 fields default without touching the file format
        assert_eq!(
            plan.acceptance[0].validation.status,
            ValidationStatus::Pending
        );
        assert!(plan.acceptance[0].validation.receipts.is_empty());
        assert_eq!(plan.applied_event, None);
        let step = &plan.steps[0];
        assert_eq!(step.step_epoch, 0);
        assert_eq!(step.validation.status, ValidationStatus::Pending);
        assert_eq!(step.refs.len(), 2);
        assert_eq!(step.refs[0].path, "src/session/mod.rs");
        assert_eq!(step.refs[0].symbol.as_deref(), Some("fn::save"));
        assert_eq!(step.refs[0].intent, RefIntent::Modify);
        assert_eq!(step.refs[1].path, "src/plain.rs");
        assert_eq!(step.refs[1].symbol, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn step_ref_objects_keep_intent() {
        let create: StepRef = serde_json::from_value(serde_json::json!({
            "path": "src/new.rs", "symbol": "Thing", "intent": "create"
        }))
        .unwrap();
        assert_eq!(create.intent, RefIntent::Create);
        assert_eq!(create.symbol.as_deref(), Some("Thing"));
        // intent defaults to modify when omitted
        let plain: StepRef = serde_json::from_value(serde_json::json!({"path": "src/x.rs"}))
            .unwrap();
        assert_eq!(plain.intent, RefIntent::Modify);
        assert_eq!(plain.symbol, None);
    }

    #[test]
    fn phase0_schema_round_trips() {
        let dir = std::env::temp_dir().join(format!("sqwai-plan-phase0-{}", new_id()));
        let mut plan = new_plan();
        plan.applied_event = Some("sess:41".to_string());
        plan.steps[0].step_epoch = 3;
        plan.steps[0].validation = Validation {
            status: ValidationStatus::Passed,
            receipts: vec![Receipt {
                session: "sess".to_string(),
                seq: 41,
                state_digest: "abc".to_string(),
                command: Some("cargo test".to_string()),
                exit: Some(0),
                at: "2026-01-01T00:00:00+00:00".to_string(),
            }],
        };
        plan.acceptance[0].validation = Validation {
            status: ValidationStatus::Waived,
            receipts: Vec::new(),
        };
        plan.steps[0].refs = vec![StepRef {
            path: "src/new.rs".to_string(),
            symbol: Some("Thing".to_string()),
            intent: RefIntent::Create,
        }];
        store(&dir, &plan).unwrap();
        let loaded = open(&dir, &plan.id).unwrap();
        assert_eq!(loaded.applied_event.as_deref(), Some("sess:41"));
        assert_eq!(loaded.steps[0].step_epoch, 3);
        assert_eq!(
            loaded.steps[0].validation.status,
            ValidationStatus::Passed
        );
        assert_eq!(loaded.steps[0].validation.receipts.len(), 1);
        assert_eq!(
            loaded.steps[0].validation.receipts[0].state_digest,
            "abc"
        );
        assert_eq!(
            loaded.acceptance[0].validation.status,
            ValidationStatus::Waived
        );
        assert_eq!(loaded.steps[0].refs[0].intent, RefIntent::Create);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn finish_leaves_validation_untouched() {
        let mut plan = new_plan();
        apply(
            &mut plan,
            Op::Start {
                id: "1".into(),
                confirm: None,
            },
            &Limits::default(),
            None,
        )
        .unwrap();
        apply(
            &mut plan,
            Op::Finish {
                id: "1".into(),
                summary: "model added".into(),
                evidence: vec![],
            },
            &Limits::default(),
            None,
        )
        .unwrap();
        // done means performed, not verified (§2.1.4)
        assert_eq!(plan.step("1").unwrap().status, StepStatus::Done);
        assert_eq!(
            plan.step("1").unwrap().validation.status,
            ValidationStatus::Pending
        );
    }

    #[test]
    fn start_rejected_while_session_holds_another_step() {
        let mut plan = new_plan();
        apply(
            &mut plan,
            Op::Start {
                id: "1".into(),
                confirm: None,
            },
            &Limits::default(),
            None,
        )
        .unwrap();
        // Same session holding step 1 cannot start step 2 (§2.2.3).
        let err = apply(
            &mut plan,
            Op::Start {
                id: "2".into(),
                confirm: None,
            },
            &Limits::default(),
            Some("1"),
        )
        .unwrap_err();
        assert_eq!(err.code, "step_busy");
        assert!(err.reason.contains('1'), "reason: {}", err.reason);
        // Re-stating the held step is not "another step": falls through to
        // the status check, which rejects with the clearer pending rule.
        let err = apply(
            &mut plan,
            Op::Start {
                id: "1".into(),
                confirm: None,
            },
            &Limits::default(),
            Some("1"),
        )
        .unwrap_err();
        assert_eq!(err.code, "step_not_pending");
    }

    #[test]
    fn start_rejected_while_plan_has_other_in_progress() {
        let mut plan = new_plan();
        apply(
            &mut plan,
            Op::Start {
                id: "1".into(),
                confirm: None,
            },
            &Limits::default(),
            None,
        )
        .unwrap();
        // No session claim, but the plan globally holds step 1: a second
        // InProgress step would make attribution ambiguous.
        let err = apply(
            &mut plan,
            Op::Start {
                id: "2".into(),
                confirm: None,
            },
            &Limits::default(),
            None,
        )
        .unwrap_err();
        assert_eq!(err.code, "step_busy");
        assert!(err.reason.contains('1'), "reason: {}", err.reason);
    }

    #[test]
    fn reopen_bumps_epoch_and_resets_validation() {
        let mut plan = new_plan();
        apply(
            &mut plan,
            Op::Start {
                id: "1".into(),
                confirm: None,
            },
            &Limits::default(),
            None,
        )
        .unwrap();
        plan.step_mut("1").unwrap().evidence.push(EvidenceRef {
            session: "test".into(),
            seq: 42,
        });
        apply(
            &mut plan,
            Op::Finish {
                id: "1".into(),
                summary: "model added".into(),
                evidence: vec![42],
            },
            &Limits::default(),
            None,
        )
        .unwrap();
        plan.step_mut("1").unwrap().validation.status = ValidationStatus::Passed;
        assert_eq!(plan.step("1").unwrap().step_epoch, 0);

        reopen_for_undo(&mut plan, "1", "reopened by undo").unwrap();

        let step = plan.step("1").unwrap();
        assert_eq!(step.status, StepStatus::Reopened);
        assert_eq!(step.step_epoch, 1);
        assert_eq!(step.validation.status, ValidationStatus::Pending);
        assert!(step.validation.receipts.is_empty());
        assert!(step.evidence.is_empty());
    }

    #[test]
    fn open_active_resolves_per_session_and_deterministically() {
        let dir = std::env::temp_dir().join(format!("sqwai-plan-resolve-{}", new_id()));
        let mut parent = new_plan();
        parent.sessions = vec!["sess-parent".into()];
        parent.created = "2026-01-01T00:00:00+00:00".to_string();
        let parent_id = parent.id.clone();
        store(&dir, &parent).unwrap();

        // Second active plan for another session (no fork: built directly).
        let mut second = new_plan();
        second.sessions = vec!["sess-child".into()];
        second.created = "2026-09-09T00:00:00+00:00".to_string();
        let child_id = second.id.clone();
        store(&dir, &second).unwrap();

        // Querying with sess-parent returns parent plan
        let resolved_parent = open_active_for_session(&dir, Some("sess-parent"))
            .unwrap()
            .unwrap();
        assert_eq!(resolved_parent.id, parent_id);

        // Querying with sess-child returns the second plan
        let resolved_child = open_active_for_session(&dir, Some("sess-child"))
            .unwrap()
            .unwrap();
        assert_eq!(resolved_child.id, child_id);

        // Querying without session deterministically picks the newest (child)
        let resolved_default = open_active(&dir).unwrap().unwrap();
        assert_eq!(resolved_default.id, child_id);

        std::fs::remove_dir_all(&dir).ok();
    }
}
