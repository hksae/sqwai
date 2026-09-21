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
/// `session`/`seq` point at the journal `verification_receipt` (or
/// `manual_confirmation`) record. Phase-3 fields are all optional so plan
/// files written before receipts keep loading unchanged.
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
    /// blake3 of the check definition (the command text). Re-running a
    /// rewritten command is a new check, never a refresh of this receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_definition_hash: Option<String>,
    /// "exec" for host-run commands, "manual" for user confirmations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// state digest before/after the run. A passing receipt always has
    /// `state_before == state_after`: a check that raced a mutation proves
    /// nothing and is never recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_after: Option<String>,
    /// blake3 of the captured check output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_hash: Option<String>,
    /// traversed paths the digest covered. A later `file_diff` on any of
    /// these marks the receipt (and its item) stale.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
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
                Wire::deserialize(de::value::MapAccessDeserializer::new(map)).map(|wire| StepRef {
                    path: wire.path,
                    symbol: wire.symbol,
                    intent: wire.intent,
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
    /// §12.12: proof that the check *discriminates* — a host run of the same
    /// check that failed on the tree as it stood before the work started.
    /// Captured at plan time, which is the only moment the pre-change tree is
    /// still the current one. Absent means the item may be read and shown,
    /// but a green run can never settle it: a check that has never failed is
    /// a smoke test, not acceptance.
    ///
    /// Deliberately not a [`Receipt`]: a receipt is invalidated when the
    /// paths it covered change, and a baseline is captured *expecting* them
    /// to change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<Baseline>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One host run of a `cmd:` acceptance item that failed, kept as the item's
/// evidence that it can fail at all (§12.12). The host runs the check itself:
/// a failing run the model reported is a claim, not a proof.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Baseline {
    pub at: String,
    /// non-zero by construction — the runs that passed are the ones this
    /// struct exists to leave unrecorded
    pub exit: i32,
    /// blake3 of the check definition the run used. Rewriting the command
    /// makes a new check, and the old baseline stops applying to it.
    pub check_definition_hash: String,
    /// blake3 of the captured output
    pub output_hash: String,
    /// first lines of that output, so the reason it failed is inspectable
    /// without a journal dig — "cannot find function foo" is a baseline,
    /// "command not found" is a typo wearing a baseline's clothes
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub head: String,
    /// state digest the failing run observed (before == after: a run that
    /// raced a mutation proves nothing and is never recorded)
    pub state_digest: String,
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

/// Expand `$name` / `${name}` in `cmd:` acceptance items against the
/// project's named verify commands (seeded by `/init`). Non-`cmd:`
/// items pass through untouched. Unknown names fail the whole create —
/// a wrong command burned at verify time costs a turn; rejected here it
/// costs nothing.
#[derive(Debug)]
pub struct UnknownVerify {
    pub names: Vec<String>,
    pub known: Vec<String>,
}

pub fn substitute_verify_commands(
    texts: Vec<String>,
    commands: &std::collections::BTreeMap<String, String>,
) -> Result<Vec<String>, UnknownVerify> {
    let mut unknown: Vec<String> = Vec::new();
    let out: Vec<String> = texts
        .into_iter()
        .map(|text| {
            if !text.trim_start().starts_with("cmd:") {
                return text;
            }
            expand_refs(&text, commands, &mut unknown)
        })
        .collect();
    if unknown.is_empty() {
        return Ok(out);
    }
    unknown.sort();
    unknown.dedup();
    let mut known: Vec<String> = commands.keys().cloned().collect();
    known.sort();
    Err(UnknownVerify {
        names: unknown,
        known,
    })
}

fn expand_refs(
    text: &str,
    commands: &std::collections::BTreeMap<String, String>,
    unknown: &mut Vec<String>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let braced = chars.peek() == Some(&'{');
        if braced {
            chars.next();
        }
        let mut name = String::new();
        while let Some(&d) = chars.peek() {
            if d.is_alphanumeric() || d == '_' || d == '-' {
                name.push(d);
                chars.next();
            } else {
                break;
            }
        }
        if braced {
            if chars.peek() == Some(&'}') {
                chars.next();
            } else {
                // unbalanced `${`: leave literally, do not invent
                out.push_str("${");
                out.push_str(&name);
                continue;
            }
        }
        if name.is_empty() {
            out.push('$');
            continue;
        }
        match commands.get(&name) {
            Some(cmd) => out.push_str(cmd),
            None => {
                unknown.push(name.clone());
                out.push('$');
                if braced {
                    out.push('{');
                }
                out.push_str(&name);
                if braced {
                    out.push('}');
                }
            }
        }
    }
    out
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
    /// Per-session replay cursors (§2.1.4, phase 2): `applied_event` above
    /// only remembers the last writer, so a crash between another session's
    /// append and store left ops where replay never looked (§13). Each
    /// session advances only its own entry; sessions with no entry replay
    /// nothing. The single cursor stays maintained alongside for old
    /// readers and as the last-writer marker.
    #[serde(default)]
    pub applied_events: std::collections::BTreeMap<String, u64>,
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

pub(crate) fn now() -> String {
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
    if plan.status == PlanStatus::Completed {
        plan.status = PlanStatus::Active;
    }
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
            for record in records.iter().filter(|record| record.seq > *cursor) {
                if record.kind == "plan"
                    && record
                        .fields
                        .get("plan_id")
                        .and_then(|value| value.as_str())
                        == Some(plan.id.as_str())
                {
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
    // §12.12: the baselines captured at create ride the intent, so replay
    // restores the proof instead of re-running the checks.
    if let Some(value) = get("baselines")
        && let Ok(baselines) = serde_json::from_value::<Vec<Option<Baseline>>>(value.clone())
    {
        set_baselines(&mut plan, baselines);
    }
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
    // §12.12: same as create — the baselines captured on accept ride the
    // record, so replay never re-runs a check that has since changed.
    if let Some(value) = fields.get("baselines")
        && let Ok(baselines) = serde_json::from_value::<Vec<Option<Baseline>>>(value.clone())
    {
        set_baselines(&mut fresh, baselines);
    }
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
            "start" | "finish" | "block" | "unblock" | "cancel" | "add" | "split" | "complete"
            | "join",
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
/// Collects the create intent plus every later op for this plan id across
/// all session journals — ordered best-effort by (ts, session, seq),
/// because there is no total-order counter — and re-applies them onto a
/// fresh base. Any Rejection, a missing create intent, or a deliberate
/// `plan_deleted` aborts to `None` and the caller quarantines the bytes.
/// `accept_proposal`-born plans are NOT rebuilt (re-running abandonment
/// has side effects on sibling plans); they quarantine as before.
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
    // the create intent this file was born from (mirrors rebuild_created's
    // field parsing; kept local so orphan handling stays untouched)
    let create_idx = records.iter().position(|(_, _, _, fields)| {
        fields.get("op").and_then(|o| o.as_str()) == Some("create")
            && fields.get("result_id").and_then(|r| r.as_str()) == Some(id)
    })?;
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
    // every later op for this plan, in best-effort global order
    for (idx, (ts, sess, seq, fields)) in records.iter().enumerate() {
        if idx == create_idx {
            continue;
        }
        let mine = fields.get("plan_id").and_then(|p| p.as_str()) == Some(id);
        if !mine {
            continue;
        }
        let after_create = (ts.as_str(), sess.as_str(), *seq)
            > (
                records[create_idx].0.as_str(),
                records[create_idx].1.as_str(),
                records[create_idx].2,
            );
        if !after_create {
            continue;
        }
        let record = crate::agent::journal::Record {
            seq: *seq,
            ts: ts.clone(),
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
    /// Host-only: a child session joins its parent's plan so its evidence
    /// attaches under session-strict resolution. Never exposed to the model
    /// (the `plan` tool refuses it); recorded journal-first like every op.
    Join {
        session: String,
    },
}

#[derive(Debug, Clone)]
pub struct Rejection {
    pub code: &'static str,
    pub reason: String,
    pub hint: String,
}

impl Rejection {
    pub(crate) fn new(
        code: &'static str,
        reason: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
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
        applied_events: Default::default(),
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
                baseline: None,
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
        } => verify_acceptance(plan, acceptance, Vec::new(), !evidence.is_empty(), None),
        Op::Complete => complete(plan),
        Op::Join { session } => join(plan, &session),
    }
}

/// Record a session's membership in the plan. Idempotent: replaying a join
/// (or racing joins under the lock) changes nothing the second time.
fn join(plan: &mut Plan, session: &str) -> Result<Applied, Rejection> {
    if !plan.sessions.iter().any(|s| s == session) {
        plan.sessions.push(session.to_string());
    }
    accept(plan, format!("session {session} joined plan {}", plan.id))
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
            format!(
                "finish, block or cancel step {other} first — or continue it instead of starting step {id}"
            ),
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
    let step = &plan.steps[index];
    if !matches!(step.status, StepStatus::Pending | StepStatus::Reopened) {
        return reject(
            plan,
            "step_not_splittable",
            format!("step {id} is {}", step.status.as_str()),
            "only a pending or reopened step can be split",
        );
    }
    if !step.evidence.is_empty() {
        return reject(
            plan,
            "step_has_evidence",
            format!("step {id} already has recorded evidence"),
            "steps with recorded evidence cannot be split or deleted",
        );
    }
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

/// Manifests that every state digest covers in addition to the traversed
/// paths: a dependency or toolchain move invalidates checks even when no
/// tracked file changed.
const STATE_MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "package.json",
    "package-lock.json",
];

/// Canonical path form for digest inputs and receipt invalidation. Refs are
/// declared with forward slashes; diffs may arrive OS-native.
fn norm_path(path: &str) -> String {
    path.replace('\\', "/").trim_start_matches("./").to_string()
}

/// Sorted union of declared step ref paths: the digest input set (§2.1.4,
/// decision 2+3). Deterministic for a fixed plan, so equal digests mean
/// nothing relevant moved.
pub fn digest_paths(plan: &Plan) -> Vec<String> {
    let mut paths = std::collections::BTreeSet::new();
    for step in &plan.steps {
        for r in &step.refs {
            paths.insert(norm_path(&r.path));
        }
    }
    paths.into_iter().collect()
}

/// blake3 state digest over traversed file contents (missing files hash as
/// a tombstone, so creation/deletion moves the digest), manifest contents,
/// and the check text. Computed before AND after a check run: only equal
/// halves make a passing receipt.
pub fn state_digest(root: &Path, paths: &[String], command: &str) -> String {
    let mut h = blake3::Hasher::new();
    for path in paths {
        h.update(path.as_bytes());
        h.update(b"\0");
        if let Ok(bytes) = std::fs::read(root.join(path)) {
            h.update(&bytes);
        } else {
            h.update(b"<missing>");
        }
        h.update(b"\0");
    }
    for manifest in STATE_MANIFESTS {
        if let Ok(bytes) = std::fs::read(root.join(manifest)) {
            h.update(manifest.as_bytes());
            h.update(b"\0");
            h.update(&bytes);
            h.update(b"\0");
        }
    }
    h.update(command.as_bytes());
    h.finalize().to_hex().to_string()
}

/// Identity of a check definition: same command text, same check.
pub fn check_definition_hash(command: &str) -> String {
    blake3::hash(command.as_bytes()).to_hex().to_string()
}

/// §12.12: does this item carry proof that its check *discriminates*?
///
/// True only for a `cmd:` item whose baseline was taken against the check
/// definition it still has — rewriting the command makes a new check, and the
/// old failing run stops being evidence about it. Manual items and free text
/// can never be proven this way, which is why they settle on other terms.
pub fn proven_failing(item: &Acceptance) -> bool {
    let AcceptanceKind::Command(command) = item.kind() else {
        return false;
    };
    item.baseline
        .as_ref()
        .is_some_and(|b| b.check_definition_hash == check_definition_hash(command))
}

/// Host-only: attach the baselines captured at plan time (§12.12). The vector
/// is positional against the acceptance list; a missing or short vector leaves
/// the remaining items without a baseline, which is a state they can be shown
/// in but never settled from.
pub fn set_baselines(plan: &mut Plan, baselines: Vec<Option<Baseline>>) {
    for (item, baseline) in plan.acceptance.iter_mut().zip(baselines) {
        item.baseline = baseline;
    }
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
    receipt: Option<Receipt>,
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
            // `complete` runs it again rather than trusting an old record.
            // Whether the check has ever been *able* to fail is a rule about
            // what the host may accept, so it lives at the host boundary
            // (`tools::verify_acceptance`) and not here: replay restores
            // commits that were already accepted, and must not be re-judged.
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
                .filter(|(other, item)| *other != index && item.status == AcceptanceStatus::Passed)
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
    // passed validation rides with the check that earned it: a host-run
    // command attaches its interval receipt, evidence-backed items record
    // the pass itself. Replay of pre-receipt commits passes `None` and
    // keeps the legacy shape (status without validation).
    if let Some(receipt) = receipt {
        item.validation.status = ValidationStatus::Passed;
        item.validation.receipts.push(receipt);
    }
    let message = if supplied_evidence {
        format!("acceptance {index} verified (model evidence ignored; host evidence used)")
    } else {
        format!("acceptance {index} verified")
    };
    accept(plan, message)
}

/// An acceptance item verified under previous rules: `Passed` status with
/// no validation block at all. New verifies always set validation, and
/// replay heals it for journaled commits — this clause is only for plan
/// files that predate receipts.
pub(crate) fn legacy_passed(item: &Acceptance) -> bool {
    item.status == AcceptanceStatus::Passed
        && item.validation.status == ValidationStatus::Pending
        && item.validation.receipts.is_empty()
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
            format!("steps still open: {}", pending.join(", ")),
            "finish, unblock or cancel them first",
        );
    }
    // §2.1.4: every item needs validation passed|waived. Stale is its own
    // rejection (state moved under a recorded check: re-verify, don't
    // complete on it); legacy passes predate receipts and count as-is.
    let pending: Vec<usize> = plan
        .acceptance
        .iter()
        .enumerate()
        .filter(|(_, a)| {
            // stale items report through the stale error below, not here
            a.validation.status != ValidationStatus::Stale
                && !matches!(
                    a.validation.status,
                    ValidationStatus::Passed | ValidationStatus::Waived
                )
                && !legacy_passed(a)
        })
        .map(|(i, _)| i)
        .collect();
    let stale: Vec<usize> = plan
        .acceptance
        .iter()
        .enumerate()
        .filter(|(_, a)| a.validation.status == ValidationStatus::Stale)
        .map(|(i, _)| i)
        .collect();
    if !pending.is_empty() {
        return reject(
            plan,
            "acceptance_pending",
            format!(
                "acceptance items still pending: {}",
                pending
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            "verify them, or have the user waive them with /plan waive. Pending acceptance items without cmd: prefix require user waiver (/plan waive <index>) or conversion to verify steps.",
        );
    }
    if !stale.is_empty() {
        return reject(
            plan,
            "acceptance_stale",
            format!(
                "acceptance items went stale after verification: {}",
                stale
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            "tracked files changed under a recorded check; re-verify the items, then complete",
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
            out.push_str(&format!("  [{}] {} {}", i, a.status.as_str(), a.text));
            // validation is the complete gate (§2.1.4): surface non-pending
            // states so a stale check is visible before `complete` rejects it
            match a.validation.status {
                ValidationStatus::Pending => {}
                other => out.push_str(&format!(" [validation: {}]", other.as_str())),
            }
            // §12.12: a cmd: item with no baseline cannot be settled by a
            // green run, so say so here rather than at the verify that fails
            if matches!(a.kind(), AcceptanceKind::Command(_)) && !proven_failing(a) {
                out.push_str(" [no baseline]");
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

    #[test]
    fn substitute_verify_commands_expands_known() {
        let mut map = std::collections::BTreeMap::new();
        map.insert("unit".to_string(), "cargo test --lib".to_string());
        map.insert("e2e".to_string(), "make test-e2e".to_string());
        let out = substitute_verify_commands(
            vec![
                "cmd: $unit".to_string(),
                "cmd: run ${e2e} now".to_string(),
                "manual: ask the user".to_string(),
                "free text $unit stays".to_string(),
            ],
            &map,
        )
        .expect("known names expand");
        assert_eq!(out[0], "cmd: cargo test --lib");
        assert_eq!(out[1], "cmd: run make test-e2e now");
        assert_eq!(out[2], "manual: ask the user");
        assert_eq!(out[3], "free text $unit stays");
    }

    #[test]
    fn substitute_verify_commands_rejects_unknown() {
        let mut map = std::collections::BTreeMap::new();
        map.insert("unit".to_string(), "cargo test --lib".to_string());
        let err = substitute_verify_commands(vec!["cmd: $nope and ${missing}".to_string()], &map)
            .expect_err("unknown names fail");
        assert_eq!(err.names, vec!["missing".to_string(), "nope".to_string()]);
        assert_eq!(err.known, vec!["unit".to_string()]);
        // empty map: still an error (fail fast, not a silent literal)
        let err = substitute_verify_commands(vec!["cmd: $x".to_string()], &Default::default())
            .expect_err("no names seeded");
        assert!(err.known.is_empty());
    }

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
    fn preferred_active_plan_rejects_dead_ids() {
        // Regression: the startup screen showed a deleted plan as active
        // because the linked id was opened without a status check.
        let dir = std::env::temp_dir().join(format!("sqwai-plan-preferred-{}", new_id()));
        assert!(preferred_active_plan(&dir, "01DOESNOTEXIST00000000000").is_none());
        let mut plan = new_plan();
        store(&dir, &plan).unwrap();
        assert_eq!(
            preferred_active_plan(&dir, &plan.id).map(|p| p.id),
            Some(plan.id.clone())
        );
        plan.status = PlanStatus::Completed;
        store(&dir, &plan).unwrap();
        assert!(preferred_active_plan(&dir, &plan.id).is_none());
        plan.status = PlanStatus::Abandoned;
        store(&dir, &plan).unwrap();
        assert!(preferred_active_plan(&dir, &plan.id).is_none());
        std::fs::remove_file(plans_dir(&dir).join(format!("{}.json", plan.id))).unwrap();
        assert!(preferred_active_plan(&dir, &plan.id).is_none());
        std::fs::remove_dir_all(&dir).ok();
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
    fn replay_heals_other_session_crash_leaving_mine_alone() {
        // §13 flagship: B's op journaled but never stored (crash between
        // append and store); the old single cursor only remembered A.
        let dir = std::env::temp_dir().join(format!("sqwai-plan-multisess-{}", new_id()));
        let mut plan = new_plan();
        // file state as after A's committed start: step running, cursors set
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
        plan.applied_event = Some("aaa:1".to_string());
        plan.applied_events.insert("aaa".to_string(), 1);
        plan.applied_events.insert("bbb".to_string(), 0);
        store(&dir, &plan).unwrap();
        // journals: A's start intent (already reflected), then B's crash —
        // a block B wrote to its own journal without storing
        let mut ja = crate::agent::journal::Journal::open(&dir, "aaa").unwrap();
        ja.append(
            "plan",
            serde_json::json!({
                "op": "start", "id": "1",
                "plan_id": plan.id, "by": "model", "ok": true,
            }),
        )
        .unwrap();
        let mut jb = crate::agent::journal::Journal::open(&dir, "bbb").unwrap();
        jb.append(
            "plan",
            serde_json::json!({
                "op": "block", "id": "1", "reason": "stuck",
                "plan_id": plan.id, "by": "model", "ok": true,
            }),
        )
        .unwrap();

        let report = replay(&dir).unwrap();
        assert_eq!(report.ops_applied, 1);
        let healed = open(&dir, &plan.id).unwrap();
        assert_eq!(healed.step("1").unwrap().status, StepStatus::Blocked);
        // both cursors advanced independently; last writer wins the marker
        assert_eq!(healed.applied_events.get("aaa"), Some(&1));
        assert_eq!(healed.applied_events.get("bbb"), Some(&1));
        assert_eq!(
            healed.applied_event.as_deref(),
            Some("bbb:1".to_string().as_str())
        );
        // idempotent again
        let again = replay(&dir).unwrap();
        assert_eq!(again.ops_applied, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn replay_stall_in_one_session_does_not_block_the_other() {
        let dir = std::env::temp_dir().join(format!("sqwai-plan-stall-{}", new_id()));
        let mut plan = new_plan();
        // cursors seeded, files clean: everything below is crash debris
        plan.applied_events.insert("aaa".to_string(), 0);
        plan.applied_events.insert("bbb".to_string(), 0);
        store(&dir, &plan).unwrap();
        // aaa (sorted first) journaled a start on a missing step: diverges
        let mut ja = crate::agent::journal::Journal::open(&dir, "aaa").unwrap();
        ja.append(
            "plan",
            serde_json::json!({
                "op": "start", "id": "99",
                "plan_id": plan.id, "by": "model", "ok": true,
            }),
        )
        .unwrap();
        // bbb journaled a valid start on step 1
        let mut jb = crate::agent::journal::Journal::open(&dir, "bbb").unwrap();
        jb.append(
            "plan",
            serde_json::json!({
                "op": "start", "id": "1",
                "plan_id": plan.id, "by": "model", "ok": true,
            }),
        )
        .unwrap();

        let report = replay(&dir).unwrap();
        assert_eq!(report.stalled, vec![plan.id.clone()]);
        let healed = open(&dir, &plan.id).unwrap();
        // bbb's stream healed despite aaa's stall; the bad cursor holds
        assert_eq!(healed.step("1").unwrap().status, StepStatus::InProgress);
        assert_eq!(healed.applied_events.get("aaa"), Some(&0));
        assert_eq!(healed.applied_events.get("bbb"), Some(&1));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn open_rebuilds_torn_plan_file_from_journal() {
        let dir = std::env::temp_dir().join(format!("sqwai-plan-rebuild-{}", new_id()));
        let id = "rebuild-me";
        // create intent in one session, a later op in another
        let mut ja = crate::agent::journal::Journal::open(&dir, "aaa").unwrap();
        ja.append(
            "plan",
            serde_json::json!({
                "op": "create", "result_id": id, "result_created": "t",
                "result_sessions": ["aaa", "bbb"],
                "goal": "ship it", "constraints": ["c1"],
                "acceptance": ["cmd: x"], "budget_limit": 1000,
                "steps": [{"title": "s1"}],
                "by": "model", "ok": true,
            }),
        )
        .unwrap();
        let mut jb = crate::agent::journal::Journal::open(&dir, "bbb").unwrap();
        jb.append(
            "plan",
            serde_json::json!({
                "op": "start", "id": "1",
                "plan_id": id, "by": "model", "ok": true,
            }),
        )
        .unwrap();
        // torn write: half a JSON document on disk
        std::fs::create_dir_all(dir.join(".sqwai/plans")).unwrap();
        std::fs::write(
            dir.join(".sqwai/plans").join(format!("{id}.json")),
            r#"{"version":1,"id":"rebuild-me","status":"act"#,
        )
        .unwrap();

        let plan = open(&dir, id).expect("torn file rebuilds");
        assert_eq!(plan.goal.text, "ship it");
        assert_eq!(plan.step("1").unwrap().status, StepStatus::InProgress);
        assert_eq!(plan.applied_events.get("aaa"), Some(&1));
        assert_eq!(plan.applied_events.get("bbb"), Some(&1));
        // nothing quarantined: the bytes were healed, not moved aside
        assert!(
            !dir.join(".sqwai/plans/corrupt")
                .join(format!("{id}.json"))
                .exists()
        );
        // and the rebuilt file loads cleanly straight away
        let again = open(&dir, id).expect("rebuilt file parses");
        assert_eq!(again.step("1").unwrap().status, StepStatus::InProgress);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn open_quarantines_when_rebuild_diverges_or_lacks_intent() {
        // diverged: create has one step, journal starts a missing one
        let dir = std::env::temp_dir().join(format!("sqwai-plan-diverge-{}", new_id()));
        let id = "diverged";
        let mut j = crate::agent::journal::Journal::open(&dir, "aaa").unwrap();
        j.append(
            "plan",
            serde_json::json!({
                "op": "create", "result_id": id, "result_created": "t",
                "result_sessions": ["aaa"],
                "goal": "g", "constraints": [], "acceptance": [],
                "budget_limit": 1000, "steps": [{"title": "s1"}],
                "by": "model", "ok": true,
            }),
        )
        .unwrap();
        j.append(
            "plan",
            serde_json::json!({
                "op": "start", "id": "99",
                "plan_id": id, "by": "model", "ok": true,
            }),
        )
        .unwrap();
        std::fs::create_dir_all(dir.join(".sqwai/plans")).unwrap();
        std::fs::write(
            dir.join(".sqwai/plans").join(format!("{id}.json")),
            "garbage{",
        )
        .unwrap();
        assert!(open(&dir, id).is_err());
        assert!(
            dir.join(".sqwai/plans/corrupt")
                .join(format!("{id}.json"))
                .exists()
        );

        // deliberate absence: plan_deleted beats any intent
        let dir2 = std::env::temp_dir().join(format!("sqwai-plan-del-{}", new_id()));
        let mut j2 = crate::agent::journal::Journal::open(&dir2, "aaa").unwrap();
        j2.append(
            "plan",
            serde_json::json!({
                "op": "create", "result_id": "gone", "result_created": "t",
                "result_sessions": ["aaa"],
                "goal": "g", "constraints": [], "acceptance": [],
                "budget_limit": 1000, "steps": [{"title": "s1"}],
                "by": "model", "ok": true,
            }),
        )
        .unwrap();
        j2.append(
            "plan",
            serde_json::json!({"op": "plan_deleted", "plan_id": "gone", "by": "host", "ok": true}),
        )
        .unwrap();
        std::fs::create_dir_all(dir2.join(".sqwai/plans")).unwrap();
        std::fs::write(dir2.join(".sqwai/plans/gone.json"), "garbage{").unwrap();
        assert!(open(&dir2, "gone").is_err());
        assert!(dir2.join(".sqwai/plans/corrupt/gone.json").exists());
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&dir2).ok();
    }

    #[test]
    fn replay_heals_crash_between_journal_and_store() {
        let dir = std::env::temp_dir().join(format!("sqwai-plan-replay-{}", new_id()));
        let mut plan = new_plan();
        plan.applied_event = Some("crash:0".to_string());
        store(&dir, &plan).unwrap();
        // The crash: intent journaled, plan file never caught up.
        let mut journal = crate::agent::journal::Journal::open(&dir, "crash").unwrap();
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
        assert_eq!(healed.step("1").unwrap().status, StepStatus::InProgress);
        assert_eq!(healed.applied_event.as_deref(), Some("crash:1"));

        // Idempotent: a second run changes nothing and stores nothing.
        let again = replay(&dir).unwrap();
        assert_eq!(again.ops_applied, 0);
        assert!(again.plans_healed.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn join_is_journal_first_idempotent_and_replayable() {
        let dir = std::env::temp_dir().join(format!("sqwai-plan-join-{}", new_id()));
        let mut plan = new_plan();
        plan.sessions = vec!["parent".to_string()];
        plan.applied_event = Some("parent:0".to_string());
        store(&dir, &plan).unwrap();
        // The crash: join intent journaled in the parent's file (where the
        // cursor points), plan file never caught up.
        let mut journal = crate::agent::journal::Journal::open(&dir, "parent").unwrap();
        journal
            .append(
                "plan",
                serde_json::json!({
                    "op": "join", "session": "sub-9",
                    "plan_id": plan.id, "by": "host", "ok": true,
                }),
            )
            .unwrap();

        let report = replay(&dir).unwrap();
        assert_eq!(report.ops_applied, 1);
        let healed = open(&dir, &plan.id).unwrap();
        assert!(healed.sessions.contains(&"sub-9".to_string()));
        assert_eq!(healed.applied_event.as_deref(), Some("parent:1"));

        // Idempotent: replay and direct apply change nothing further.
        let again = replay(&dir).unwrap();
        assert_eq!(again.ops_applied, 0);
        let mut healed = healed;
        apply(
            &mut healed,
            Op::Join {
                session: "sub-9".to_string(),
            },
            &Limits::default(),
            None,
        )
        .unwrap();
        assert_eq!(healed.sessions.iter().filter(|s| *s == "sub-9").count(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn replay_rebuilds_orphan_create_and_respects_delete() {
        let dir = std::env::temp_dir().join(format!("sqwai-plan-orphan-{}", new_id()));
        let mut journal = crate::agent::journal::Journal::open(&dir, "orphan").unwrap();
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
        std::fs::remove_file(plans_dir(&dir).join("01J000ORPHAN00000000000001.json")).unwrap();
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
        assert_eq!(
            open(&dir, &plan.id).unwrap().step("1").unwrap().status,
            StepStatus::Pending
        );
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
            journal
                .append("note", serde_json::json!({"note": "x", "kind": "decision"}))
                .unwrap();
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
        let plain: StepRef =
            serde_json::from_value(serde_json::json!({"path": "src/x.rs"})).unwrap();
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
                check_definition_hash: None,
                runner: None,
                args: None,
                cwd: None,
                started_at: None,
                finished_at: None,
                state_before: None,
                state_after: None,
                output_hash: None,
                paths: Vec::new(),
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
        assert_eq!(loaded.steps[0].validation.status, ValidationStatus::Passed);
        assert_eq!(loaded.steps[0].validation.receipts.len(), 1);
        assert_eq!(loaded.steps[0].validation.receipts[0].state_digest, "abc");
        assert_eq!(
            loaded.acceptance[0].validation.status,
            ValidationStatus::Waived
        );
        assert_eq!(loaded.steps[0].refs[0].intent, RefIntent::Create);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn receipt(paths: &[&str]) -> Receipt {
        Receipt {
            session: "sess".to_string(),
            seq: 7,
            state_digest: "digest".to_string(),
            command: Some("cmd: check".to_string()),
            exit: Some(0),
            at: now(),
            check_definition_hash: Some(check_definition_hash("cmd: check")),
            runner: Some("exec".to_string()),
            args: None,
            cwd: None,
            started_at: None,
            finished_at: None,
            state_before: Some("digest".to_string()),
            state_after: Some("digest".to_string()),
            output_hash: Some("outhash".to_string()),
            paths: paths.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn close_steps(plan: &mut Plan) {
        for step in plan.steps.iter_mut() {
            step.status = StepStatus::Done;
        }
    }

    #[test]
    fn state_digest_tracks_content_and_tombstones() {
        let dir = std::env::temp_dir().join(format!("sqwai-plan-digest-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "one").unwrap();
        let d1 = state_digest(&dir, &["a.rs".to_string()], "cmd");
        // the command text participates: same tree, other check, other digest
        assert_ne!(d1, state_digest(&dir, &["a.rs".to_string()], "other"));
        std::fs::write(dir.join("a.rs"), "two").unwrap();
        let d2 = state_digest(&dir, &["a.rs".to_string()], "cmd");
        assert_ne!(d1, d2);
        // deletion is a tombstone, distinct from every content
        std::fs::remove_file(dir.join("a.rs")).unwrap();
        let d3 = state_digest(&dir, &["a.rs".to_string()], "cmd");
        assert_ne!(d2, d3);
        assert_ne!(d1, d3);
        assert_eq!(d3, state_digest(&dir, &["a.rs".to_string()], "cmd"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn verify_attaches_receipt_and_stale_blocks_complete_until_reverified() {
        let mut plan = new_plan();
        close_steps(&mut plan);
        verify_acceptance(&mut plan, 0, vec![], false, Some(receipt(&["x.rs"]))).unwrap();
        assert_eq!(plan.acceptance[0].status, AcceptanceStatus::Passed);
        assert_eq!(
            plan.acceptance[0].validation.status,
            ValidationStatus::Passed
        );
        assert_eq!(plan.acceptance[0].validation.receipts.len(), 1);
        // an unrelated diff changes nothing
        assert!(!apply_invalidate(&mut plan, &["other.rs".to_string()]));
        // a diff on traversed paths stales the item and blocks complete
        assert!(apply_invalidate(&mut plan, &["x.rs".to_string()]));
        assert_eq!(
            plan.acceptance[0].validation.status,
            ValidationStatus::Stale
        );
        assert!(
            render(&plan).contains("[validation: stale]"),
            "stale must surface before complete rejects it"
        );
        let err = apply(&mut plan, Op::Complete, &Limits::default(), None).unwrap_err();
        assert_eq!(err.code, "acceptance_stale");
        // re-verifying heals with a second receipt; history accumulates
        verify_acceptance(&mut plan, 0, vec![], false, Some(receipt(&["x.rs"]))).unwrap();
        assert_eq!(plan.acceptance[0].validation.receipts.len(), 2);
        assert!(matches!(
            apply(&mut plan, Op::Complete, &Limits::default(), None),
            Ok(Applied::Completed)
        ));
    }

    #[test]
    fn waived_validation_survives_invalidate() {
        let mut plan = new_plan();
        waive(&mut plan, 0, "not now").unwrap();
        assert_eq!(
            plan.acceptance[0].validation.status,
            ValidationStatus::Waived
        );
        assert!(!apply_invalidate(&mut plan, &["anything.rs".to_string()]));
        assert_eq!(
            plan.acceptance[0].validation.status,
            ValidationStatus::Waived
        );
    }

    /// Mirrors `Acceptance::kind()`: the hash is taken over the *stripped*
    /// command, which is the text the host actually runs.
    fn baseline_for(item_text: &str) -> Baseline {
        let command = item_text.strip_prefix("cmd:").unwrap_or(item_text).trim();
        Baseline {
            at: now(),
            exit: 101,
            check_definition_hash: check_definition_hash(command),
            output_hash: "outhash".to_string(),
            head: "error[E0425]: cannot find function `foo`".to_string(),
            state_digest: "digest".to_string(),
        }
    }

    #[test]
    fn baseline_proves_failure_only_against_the_check_it_was_taken_on() {
        let mut plan = new_plan(); // acceptance[0] is "cmd: cargo test"
        assert!(!proven_failing(&plan.acceptance[0]), "no baseline yet");
        set_baselines(&mut plan, vec![Some(baseline_for("cmd: cargo test"))]);
        assert!(proven_failing(&plan.acceptance[0]));
        // rewriting the command makes a new check: the old failing run stops
        // being evidence about it, which is the whole point of freezing it
        plan.acceptance[0].text = "cmd: cargo test --lib".to_string();
        assert!(!proven_failing(&plan.acceptance[0]));
    }

    #[test]
    fn baseline_never_proves_a_manual_or_text_item() {
        let mut plan = create(
            "render the page".to_string(),
            Vec::new(),
            vec![
                "manual: eyeball it".to_string(),
                "the page renders".to_string(),
            ],
            vec![NewStep {
                title: "do the work".to_string(),
                kind: None,
                refs: Vec::new(),
            }],
            0,
            &Limits::default(),
        )
        .unwrap();
        // even handed the same shape of proof, neither kind can be settled by
        // a host run: only a command is runnable
        set_baselines(
            &mut plan,
            vec![
                Some(baseline_for("manual: eyeball it")),
                Some(baseline_for("the page renders")),
            ],
        );
        assert!(!proven_failing(&plan.acceptance[0]));
        assert!(!proven_failing(&plan.acceptance[1]));
    }

    #[test]
    fn set_baselines_is_positional_and_a_short_vector_leaves_the_rest_unproven() {
        let mut plan = create(
            "two checks".to_string(),
            Vec::new(),
            vec!["cmd: cargo test".to_string(), "cmd: cargo clippy".to_string()],
            vec![NewStep {
                title: "do the work".to_string(),
                kind: None,
                refs: Vec::new(),
            }],
            0,
            &Limits::default(),
        )
        .unwrap();
        set_baselines(&mut plan, vec![Some(baseline_for("cmd: cargo test"))]);
        assert!(proven_failing(&plan.acceptance[0]));
        assert!(
            !proven_failing(&plan.acceptance[1]),
            "an item past the end of the vector stays unproven"
        );
    }

    #[test]
    fn render_marks_a_cmd_item_that_cannot_settle_anything() {
        let mut plan = new_plan();
        assert!(render(&plan).contains("[no baseline]"));
        set_baselines(&mut plan, vec![Some(baseline_for("cmd: cargo test"))]);
        assert!(!render(&plan).contains("[no baseline]"));
    }

    #[test]
    fn an_acceptance_item_without_a_baseline_still_loads() {
        // plan files written before §12.12 carry no baseline at all
        let item: Acceptance = serde_json::from_value(serde_json::json!({
            "text": "cmd: cargo test",
            "status": "pending",
        }))
        .unwrap();
        assert!(item.baseline.is_none());
        assert!(!proven_failing(&item));
    }

    #[test]
    fn legacy_passed_without_validation_still_completes() {
        // plan files written before receipts carry status without validation
        let mut plan = new_plan();
        close_steps(&mut plan);
        plan.acceptance[0].status = AcceptanceStatus::Passed;
        assert!(matches!(
            apply(&mut plan, Op::Complete, &Limits::default(), None),
            Ok(Applied::Completed)
        ));
    }

    #[test]
    fn confirm_manual_records_point_in_time_receipt() {
        let dir = std::env::temp_dir().join(format!("sqwai-plan-confirm-{}", new_id()));
        let mut plan = create(
            "goal".to_string(),
            Vec::new(),
            vec!["manual: eyeball it".to_string()],
            vec![NewStep {
                title: "t".to_string(),
                kind: None,
                refs: Vec::new(),
            }],
            20000,
            &Limits::default(),
        )
        .expect("plan creates");
        confirm(&dir, "sess", &mut plan, 0, "looks good").unwrap();
        assert_eq!(plan.acceptance[0].status, AcceptanceStatus::Passed);
        assert_eq!(
            plan.acceptance[0].validation.status,
            ValidationStatus::Passed
        );
        let receipts = &plan.acceptance[0].validation.receipts;
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].runner.as_deref(), Some("manual"));
        assert_eq!(receipts[0].state_before, receipts[0].state_after);
        // the confirmation itself is journaled
        let records = crate::agent::journal::Journal::records_for(&dir, "sess").unwrap();
        assert!(records.iter().any(|r| r.kind == "manual_confirmation"
            && r.fields.get("acceptance_id").and_then(|v| v.as_u64()) == Some(0)));
        // commands and evidence-backed text refuse: they have own paths
        let mut cmd_plan = new_plan();
        let err = confirm(&dir, "sess", &mut cmd_plan, 0, "trust me").unwrap_err();
        assert_eq!(err.code, "not_manual");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stale_announcements_fire_once_per_stale_item() {
        let plan_with = |statuses: &[&str]| {
            let items: Vec<String> = statuses
                .iter()
                .map(|s| {
                    format!(
                        r#"{{"text":"cmd: x","status":"pending","validation":{{"status":"{s}"}}}}"#
                    )
                })
                .collect();
            serde_json::from_str::<Plan>(&format!(
                r#"{{"version":1,"id":"p","status":"active","created":"t",
                    "goal":{{"text":"g","source":"user","created":"t"}},
                    "budget":{{"tokens":0,"limit":0}},"revision":1,
                    "steps":[],"acceptance":[{}]}}"#,
                items.join(",")
            ))
            .expect("test plan must parse")
        };
        let empty = std::collections::HashSet::new();
        // item 1 stale, item 0 passed: only 1 announces
        let (fresh, next) = stale_announcements(&empty, &plan_with(&["passed", "stale"]));
        assert_eq!(fresh, vec![1]);
        // same state again: silence (already announced)
        let (fresh2, next2) = stale_announcements(&next, &plan_with(&["passed", "stale"]));
        assert!(fresh2.is_empty());
        // re-verified then stale again: announces again
        let (_, next3) = stale_announcements(&next2, &plan_with(&["passed", "passed"]));
        let (fresh4, _) = stale_announcements(&next3, &plan_with(&["passed", "stale"]));
        assert_eq!(fresh4, vec![1]);
        // foreign plan entries prune without announcing
        let mut foreign = std::collections::HashSet::new();
        foreign.insert(("other".to_string(), 0));
        let (fresh5, next5) = stale_announcements(&foreign, &plan_with(&["passed"]));
        assert!(fresh5.is_empty());
        assert!(next5.is_empty());
    }

    #[test]
    fn invalidate_on_diff_commits_only_on_change() {
        let dir = std::env::temp_dir().join(format!("sqwai-plan-inv-{}", new_id()));
        let mut plan = new_plan();
        // #171: resolution is session-strict — the plan must carry the
        // session under test, as plan_op sets on create in prod
        plan.sessions = vec!["sess".to_string()];
        close_steps(&mut plan);
        plan.steps[0].refs = vec![StepRef::from("x.rs")];
        verify_acceptance(&mut plan, 0, vec![], false, Some(receipt(&["x.rs"]))).unwrap();
        store(&dir, &plan).unwrap();
        assert!(invalidate_on_diff(&dir, "sess", &["x.rs".to_string()]).unwrap());
        let reloaded = open_active_for_session(&dir, Some("sess"))
            .unwrap()
            .expect("active plan");
        assert_eq!(
            reloaded.acceptance[0].validation.status,
            ValidationStatus::Stale
        );
        assert!(reloaded.applied_event.is_some(), "invalidation commits");
        // unrelated paths: no commit, cursor untouched
        let cursor = reloaded.applied_event.clone();
        assert!(!invalidate_on_diff(&dir, "sess", &["y.rs".to_string()]).unwrap());
        let same = open_active_for_session(&dir, Some("sess"))
            .unwrap()
            .expect("active plan");
        assert_eq!(same.applied_event, cursor);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn replay_restores_verify_receipt_from_commit_args() {
        let dir = std::env::temp_dir().join(format!("sqwai-plan-repreceipt-{}", new_id()));
        let mut plan = new_plan();
        close_steps(&mut plan);
        plan.applied_event = Some("rr:0".to_string());
        store(&dir, &plan).unwrap();
        let receipt_value = serde_json::to_value(receipt(&["x.rs"])).unwrap();
        let mut journal = crate::agent::journal::Journal::open(&dir, "rr").unwrap();
        journal
            .append(
                "plan",
                serde_json::json!({
                    "op": "verify", "acceptance": 0, "evidence_refs": [],
                    "receipt": receipt_value,
                    "plan_id": plan.id, "by": "model", "ok": true,
                }),
            )
            .unwrap();
        replay(&dir).unwrap();
        let healed = open(&dir, &plan.id).unwrap();
        assert_eq!(
            healed.acceptance[0].validation.status,
            ValidationStatus::Passed
        );
        assert_eq!(healed.acceptance[0].validation.receipts.len(), 1);
        assert_eq!(
            healed.acceptance[0].validation.receipts[0]
                .runner
                .as_deref(),
            Some("exec")
        );
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
    fn reopen_for_undo_reactivates_completed_plan() {
        let mut plan = new_plan();
        plan.status = PlanStatus::Completed;
        plan.steps[0].status = StepStatus::Done;
        reopen_for_undo(&mut plan, "1", "undo step 1").unwrap();
        assert_eq!(plan.steps[0].status, StepStatus::Reopened);
        assert_eq!(plan.status, PlanStatus::Active);
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

    /// #171: a session with no own active plan resolves to None — never to
    /// another session's plan. Silent cross-session adoption is gone.
    #[test]
    fn open_active_for_session_never_returns_a_foreign_plan() {
        let dir = std::env::temp_dir().join(format!("sqwai-plan-strict-{}", new_id()));
        let mut plan = new_plan();
        plan.sessions = vec!["owner".into()];
        store(&dir, &plan).unwrap();

        assert!(
            open_active_for_session(&dir, Some("stranger"))
                .unwrap()
                .is_none(),
            "a plan-less session must not adopt the foreign plan"
        );
        // ...while the owner still resolves, and the global view is intact
        assert!(
            open_active_for_session(&dir, Some("owner"))
                .unwrap()
                .is_some()
        );
        assert!(open_active(&dir).unwrap().is_some());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn split_pending_or_reopened_step_succeeds() {
        let mut plan = new_plan();
        let parts = vec![
            NewStep {
                title: "part a".into(),
                kind: None,
                refs: vec![],
            },
            NewStep {
                title: "part b".into(),
                kind: None,
                refs: vec![],
            },
        ];
        assert!(
            apply(
                &mut plan,
                Op::Split {
                    id: "1".into(),
                    into: parts.clone()
                },
                &Limits::default(),
                None,
            )
            .is_ok()
        );
        assert!(plan.step("1a").is_some());
        assert!(plan.step("1b").is_some());
        assert!(plan.step("1").is_none());

        // Reopened step can also be split
        plan.steps[2].status = StepStatus::Reopened;
        assert!(
            apply(
                &mut plan,
                Op::Split {
                    id: "2".into(),
                    into: parts
                },
                &Limits::default(),
                None,
            )
            .is_ok()
        );
        assert!(plan.step("2a").is_some());
        assert!(plan.step("2b").is_some());
    }

    #[test]
    fn split_rejects_done_in_progress_blocked_or_evidenced_steps() {
        let mut plan = new_plan();
        let parts = vec![
            NewStep {
                title: "part a".into(),
                kind: None,
                refs: vec![],
            },
            NewStep {
                title: "part b".into(),
                kind: None,
                refs: vec![],
            },
        ];

        // 1. Done step cannot be split
        plan.steps[0].status = StepStatus::Done;
        let err = apply(
            &mut plan,
            Op::Split {
                id: "1".into(),
                into: parts.clone(),
            },
            &Limits::default(),
            None,
        )
        .unwrap_err();
        assert_eq!(err.code, "step_not_splittable");

        // 2. InProgress step cannot be split
        plan.steps[0].status = StepStatus::InProgress;
        let err = apply(
            &mut plan,
            Op::Split {
                id: "1".into(),
                into: parts.clone(),
            },
            &Limits::default(),
            None,
        )
        .unwrap_err();
        assert_eq!(err.code, "step_not_splittable");

        // 3. Blocked step cannot be split
        plan.steps[0].status = StepStatus::Blocked;
        let err = apply(
            &mut plan,
            Op::Split {
                id: "1".into(),
                into: parts.clone(),
            },
            &Limits::default(),
            None,
        )
        .unwrap_err();
        assert_eq!(err.code, "step_not_splittable");

        // 4. Pending step with recorded evidence cannot be split
        plan.steps[0].status = StepStatus::Pending;
        plan.steps[0].evidence.push(EvidenceRef {
            session: "sess".into(),
            seq: 1,
        });
        let err = apply(
            &mut plan,
            Op::Split {
                id: "1".into(),
                into: parts,
            },
            &Limits::default(),
            None,
        )
        .unwrap_err();
        assert_eq!(err.code, "step_has_evidence");
    }
}
