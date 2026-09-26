//! Structured plan (DESIGN §2.1).
//!
//! A plan is a host-owned document: the model reaches it only through the
//! `plan` tool operations validated here. The model can never write `goal`,
//! `constraints`, `acceptance[].status`, `evidence` or `folded` directly.
//!

use anyhow::Result;
use chrono::Local;
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use uuid::Uuid;

mod ops;
mod render;
mod store;
mod verify;
pub use ops::{
    Applied, Limits, NewStep, Op, PlanDraftArgs, Rejection, abandon,
    apply, create, reset_discards, validate_proposal_invariants, validate_surrender_reason,
};
pub(crate) use ops::{accept, add_acceptance, reject};
pub use render::{
    apply_flaky, apply_invalidate, attach_confirmation, confirm, invalidate_on_diff,
    render, render_goal, render_status, set_goal, stale_announcements, waive,
};
pub use store::{
    StepContext, commit, list, list_active, open, open_active,
    open_active_for_session, preferred_active_plan, read_plan_file, replay, store,
};
pub use verify::{
    changed_check_inputs, check_definition_hash, differential_current,
    digest_paths, freeze_check_inputs, frozen_input_paths, ladder_note, ladder_rung,
    proven_failing, set_baselines, set_inputs, set_shapes, set_snapshots,
    signatures_current, snapshot_current, state_digest, verify_acceptance,
};
pub(crate) use verify::{complete, next_id};
#[cfg(test)]
pub(crate) use verify::{Rung, ladder_top, legacy_passed};

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
    /// Honest terminal state: the task is impossible as specified (spec
    /// conflict, quoted in `blocked_reason`), not failed work. Closed like
    /// every non-active status — read-only except `show`.
    Blocked,
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
///
/// `Unknown` is the third ULTRA state (§12.12): repeated runs of the same
/// check disagreed on the same state, so the item is flaky — neither
/// verified nor plain failed. It is never retried into `passed` by another
/// green run; the user waives it or the check is made deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ValidationStatus {
    #[default]
    Pending,
    Passed,
    Stale,
    Unknown,
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
            Self::Unknown => "unknown",
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
    /// §12.12, judge ladder rung 4: the output a `snapshot:` check produced
    /// on the pre-change tree. A later run settles the item iff its output
    /// is byte-identical. Absent means the item may be read and shown, but
    /// no run can settle it: unfrozen behavior has nothing to compare to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Snapshot>,
    /// Rung 5: the declaration shapes a `signatures:` item named on the
    /// pre-change tree. A later read settles the item iff every shape is
    /// identical — bodies may move, the structure must not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<ShapeFreeze>,
    /// Frozen check inputs for `cmd:`/`snapshot:`/`differential:` items:
    /// test and fixture files hashed at plan time. Editing a listed file
    /// invalidates later verdicts (write-time refusal, receipt-time
    /// comparison); waiving the item unfreezes its inputs. Empty for kinds
    /// without commands.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<CheckInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One frozen check input: a test/fixture file hashed at plan time, so
/// editing the check itself (rather than the code under it) invalidates
/// every later verdict. Only files that existed at capture are listed —
/// new test files are always allowed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckInput {
    /// project-relative, forward slashes
    pub path: String,
    /// blake3 of the file bytes at capture
    pub hash: String,
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

/// Frozen declaration shapes for a `signatures:` acceptance item (judge
/// ladder rung 5, §12.12). One entry per named file: the normalized shape
/// (sorted `depth::signature` lines, line numbers dropped) hashed, plus
/// how it was read. Bodies may move freely; adding, removing, or
/// re-signing a declaration breaks the freeze.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShapeFreeze {
    pub at: String,
    /// blake3 of the item text (`signatures: ...` paths). Renaming the
    /// file set makes a new check, and the old shapes stop applying to it.
    pub check_definition_hash: String,
    pub files: Vec<ShapeFile>,
}

/// One frozen file inside a [`ShapeFreeze`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShapeFile {
    /// project-relative path as named in the item
    pub path: String,
    /// `ts:<lang>` or `fallback` — how the shape was read
    pub parser: String,
    /// blake3 of the normalized shape lines joined
    pub shape_hash: String,
    /// declaration count, for inspectable notes
    pub items: usize,
}

/// Frozen behavior of a `snapshot:` acceptance item (§12.12, judge ladder
/// rung 4). The host runs the check at plan time and keeps its output: a
/// later run settles the item iff the output is byte-identical. Unlike a
/// [`Baseline`], any exit code freezes — erroring the same way is behavior
/// too — but an empty output never freezes: it discriminates nothing, the
/// same way an already-passing check proves nothing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snapshot {
    pub at: String,
    pub exit: Option<i32>,
    /// blake3 of the check definition the frozen run used. Rewriting the
    /// command makes a new check, and the old output stops applying to it.
    pub check_definition_hash: String,
    /// blake3 of the frozen output
    pub output_hash: String,
    /// first lines of that output, so what was frozen stays inspectable
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub head: String,
    /// state digest the frozen run observed (before == after: a run that
    /// raced a mutation freezes nothing and is never recorded)
    pub state_digest: String,
}

/// How the host is meant to settle an acceptance item (§2.1.2).
#[derive(Debug, Clone, PartialEq)]
pub enum AcceptanceKind<'a> {
    /// `cmd: <command>` — the host runs it and the result is the verification
    Command(&'a str),
    /// `snapshot: <command>` — the host froze its output at plan time and
    /// re-runs it; byte-identical output is the verification
    Snapshot(&'a str),
    /// `differential: <command>` — the host froze its output at plan time
    /// and re-runs it; observably *changed* output is the verification
    /// (rung 3 of the judge ladder: the same input through the old and
    /// new code paths, outputs compared)
    Differential(&'a str),
    /// `signatures: <path>, ...` — the host froze the declaration shapes
    /// of the named files at plan time and re-reads them; identical shapes
    /// are the verification (rung 5: structure holds while bodies move)
    Signatures(Vec<&'a str>),
    /// `manual: <text>` — no command can settle it; the user waives it
    Manual(&'a str),
    /// free text — settled by host-recorded evidence from a verify step
    Text(&'a str),
}

impl Acceptance {
    /// Classify by prefix. Unprefixed text is `Text`, per §2.1.2.
    /// `signatures:` paths are comma-separated (`a.rs, b.rs`).
    pub fn kind(&self) -> AcceptanceKind<'_> {
        AcceptanceKind::classify(&self.text)
    }
}

impl<'a> AcceptanceKind<'a> {
    /// Classify raw item text without building an [`Acceptance`].
    pub fn classify(text: &'a str) -> AcceptanceKind<'a> {
        let text = text.trim();
        if let Some(command) = text.strip_prefix("cmd:") {
            AcceptanceKind::Command(command.trim())
        } else if let Some(command) = text.strip_prefix("snapshot:") {
            AcceptanceKind::Snapshot(command.trim())
        } else if let Some(command) = text.strip_prefix("differential:") {
            AcceptanceKind::Differential(command.trim())
        } else if let Some(paths) = text.strip_prefix("signatures:") {
            AcceptanceKind::Signatures(signature_paths(paths))
        } else if let Some(rest) = text.strip_prefix("manual:") {
            AcceptanceKind::Manual(rest.trim())
        } else {
            AcceptanceKind::Text(text)
        }
    }
}

/// Split a `signatures:` path list (`a.rs, b.rs`) into trimmed names.
/// Shared by classification and the host, so both read the same set.
pub fn signature_paths(list: &str) -> Vec<&str> {
    list.split(',')
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .collect()
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
    /// Non-blocking checklist: free-text notes from create (plan-lite).
    /// Visible in show and the panel, never gates complete — walked past,
    /// never settled. Immutable after create (no op touches it), so it
    /// rides the cached plan block.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checklist: Vec<String>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub folded: Vec<Folded>,
    pub budget: Budget,
    pub revision: u64,
    #[serde(default)]
    pub rejections_in_a_row: u32,
    /// The quoted conflict that blocked this plan (`BlockPlan`), if any.
    /// Read by `/plan show` and the bench harness; old files simply lack it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
    /// Typed-constraint indices the user waived, with reasons. Constraints
    /// never reorder (only a full replacement resets them), so indices are
    /// stable for the plan's lifetime.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waived_constraints: Vec<WaivedConstraint>,
}

/// One user-waived constraint: which, and why.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WaivedConstraint {
    pub index: usize,
    pub reason: String,
}

/// A constraint the host can execute (§2.1.10). Unprefixed constraints
/// stay advisory (claim-lint territory); only these four settle or block.
#[derive(Debug, Clone, PartialEq)]
pub enum ConstraintKind<'a> {
    /// `forbid-import: <pattern>` — no source file may import it
    ForbidImport(&'a str),
    /// `forbid-cmd: <pattern>` — the agent may not run matching commands
    ForbidCmd(&'a str),
    /// `ast: <pattern>` — the tree-sitter pattern must match nowhere
    Ast(&'a str),
    /// `path: <roots...>` — the outcome diff touches only these roots
    Path(Vec<&'a str>),
    /// anything else: documented intent, enforced by nothing
    Plain(&'a str),
}

/// Classify raw constraint text. `path:` roots are comma-separated.
pub fn classify_constraint(text: &str) -> ConstraintKind<'_> {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix("forbid-import:") {
        ConstraintKind::ForbidImport(rest.trim())
    } else if let Some(rest) = text.strip_prefix("forbid-cmd:") {
        ConstraintKind::ForbidCmd(rest.trim())
    } else if let Some(rest) = text.strip_prefix("ast:") {
        ConstraintKind::Ast(rest.trim())
    } else if let Some(roots) = text.strip_prefix("path:") {
        ConstraintKind::Path(
            roots
                .split(',')
                .map(str::trim)
                .filter(|root| !root.is_empty())
                .collect(),
        )
    } else {
        ConstraintKind::Plain(text)
    }
}

/// User waives a typed constraint (§2.1.10). Host-only, like acceptance
/// waiver: false-positive patterns must never wedge `complete` shut.
/// Idempotent — waiving twice keeps the first reason.
pub fn waive_constraint(plan: &mut Plan, index: usize, reason: &str) -> Result<(), Rejection> {
    if index >= plan.constraints.len() {
        return Err(Rejection::new(
            "unknown_constraint",
            format!("no constraint {index}"),
            "call /plan to see the constraints list",
        ));
    }
    if !plan
        .waived_constraints
        .iter()
        .any(|w| w.index == index)
    {
        plan.waived_constraints.push(WaivedConstraint {
            index,
            reason: reason.to_string(),
        });
        plan.waived_constraints.sort_by_key(|w| w.index);
    }
    plan.revision += 1;
    Ok(())
}

/// Waived constraint indices, for evaluators to skip.
pub fn waived_constraint_indices(plan: &Plan) -> Vec<usize> {
    plan.waived_constraints.iter().map(|w| w.index).collect()
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





#[cfg(test)]
mod tests {
    use super::*;

    /// The cache split, nailed down: goal + constraints never mention step
    /// state, and step transitions never touch the goal block. A step that
    /// finishes must leave `render_goal` byte-identical.
    #[test]
    fn render_splits_stable_goal_from_moving_status() {
        let mut plan = new_plan();
        let goal_before = render_goal(&plan);
        assert!(goal_before.contains("persist the plan on disk"), "{goal_before}");
        assert!(goal_before.contains("no new dependencies"), "{goal_before}");
        assert!(!goal_before.contains("add the model"), "{goal_before}");
        assert!(!goal_before.contains("active"), "{goal_before}");

        let status_before = render_status(&plan);
        assert!(status_before.contains("active"), "{status_before}");
        assert!(status_before.contains("add the model"), "{status_before}");

        plan.steps[0].status = StepStatus::Done;
        plan.steps[1].status = StepStatus::InProgress;
        assert_eq!(render_goal(&plan), goal_before, "step moves re-key nothing cached");
        assert_ne!(render_status(&plan), status_before, "step moves surface");

        // the full render still carries both halves
        let full = render(&plan);
        assert!(full.contains("persist the plan on disk"), "{full}");
        assert!(full.contains("[x] 1 add the model"), "{full}");
    }

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
                    refs: Vec::new(),
                },
                NewStep {
                    title: "add the validator".to_string(),
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
    fn closed_plans_are_read_only_except_show() {
        // defect A: the model path could keep mutating completed or
        // abandoned plans; only the TUI refused them.
        let mut plan = new_plan();
        plan.status = PlanStatus::Completed;
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
        assert_eq!(err.code, "plan_closed");
        assert!(matches!(
            apply(&mut plan, Op::Show, &Limits::default(), None),
            Ok(Applied::Shown { .. })
        ));
        plan.status = PlanStatus::Abandoned;
        let err = apply(
            &mut plan,
            Op::Finish {
                id: "1".into(),
                summary: "x".into(),
                evidence: vec![],
            },
            &Limits::default(),
            None,
        )
        .unwrap_err();
        assert_eq!(err.code, "plan_closed");
    }

    #[test]
    fn cancel_needs_a_step_id_and_never_abandons() {
        let mut plan = new_plan();
        let err = apply(
            &mut plan,
            Op::Cancel {
                id: None,
                reason: String::new(),
            },
            &Limits::default(),
            None,
        )
        .unwrap_err();
        assert_eq!(err.code, "need_step_id");
        assert_eq!(plan.status, PlanStatus::Active);

        let id = plan.id.clone();
        let err = apply(
            &mut plan,
            Op::Cancel {
                id: Some(id),
                reason: String::new(),
            },
            &Limits::default(),
            None,
        )
        .unwrap_err();
        assert_eq!(err.code, "abandon_user_only");
        assert_eq!(plan.status, PlanStatus::Active);
    }

    #[test]
    fn block_plan_records_the_quoted_conflict_and_closes() {
        let mut plan = new_plan();
        // reset refusals first (empty and thin reasons change nothing);
        // the successful abandon path is covered by the approval-flow test
        let err = apply(
            &mut plan,
            Op::ProposeReset {
                reason: "   ".to_string(),
            },
            &Limits::default(),
            None,
        )
        .unwrap_err();
        assert_eq!(err.code, "empty_reason");
        let err = apply(
            &mut plan,
            Op::ProposeReset {
                reason: "nope".to_string(),
            },
            &Limits::default(),
            None,
        )
        .unwrap_err();
        assert_eq!(err.code, "thin_reason");
        assert_eq!(plan.status, PlanStatus::Active, "refusals change nothing");
        let err = apply(
            &mut plan,
            Op::BlockPlan {
                reason: "   ".to_string(),
            },
            &Limits::default(),
            None,
        )
        .unwrap_err();
        assert_eq!(err.code, "empty_reason");

        let quote = "spec says 404, test test_missing expects 200";
        assert!(matches!(
            apply(
                &mut plan,
                Op::BlockPlan {
                    reason: quote.to_string(),
                },
                &Limits::default(),
                None,
            ),
            Ok(Applied::Updated { .. })
        ));
        assert_eq!(plan.status, PlanStatus::Blocked);
        assert_eq!(plan.blocked_reason.as_deref(), Some(quote));
        assert!(render(&plan).contains("blocked: spec says 404"));

        // terminal like every closed plan: read-only except show
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
        assert_eq!(err.code, "plan_closed");
        assert!(matches!(
            apply(&mut plan, Op::Show, &Limits::default(), None),
            Ok(Applied::Shown { .. })
        ));
    }

    /// A quoted reset abandons (history kept as Abandoned, never deleted);
    /// the discard summary counts what leaves the active surface.
    #[test]
    fn propose_reset_abandons_on_a_quoted_defect() {
        let mut plan = new_plan();
        let out = reset_discards(&plan);
        assert!(out.contains("0 steps done"), "{out}");
        assert!(matches!(
            apply(
                &mut plan,
                Op::ProposeReset {
                    reason: "goal targets removed feature X, steps assume the deleted API".to_string(),
                },
                &Limits::default(),
                None,
            ),
            Ok(Applied::Updated { .. })
        ));
        assert_eq!(plan.status, PlanStatus::Abandoned);
        // closed plans stay read-only except show — same as blocked
        assert!(matches!(
            apply(&mut plan, Op::Show, &Limits::default(), None),
            Ok(Applied::Shown { .. })
        ));
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
            err.hint.contains("Pending acceptance items without cmd:/manual: prefix require user waiver (/plan waive <index>) or conversion to steps with host evidence."),
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
            checklist: vec![],
            steps: vec![NewStep {
                title: "step 1".into(),
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

    /// Late-added criteria survive a crash rebuild with their proof: items
    /// re-append from the intent, baselines restore from the riding vectors
    /// instead of re-running checks mid-work.
    #[test]
    fn replay_restores_added_acceptance_with_proof() {
        let dir = std::env::temp_dir().join(format!("sqwai-plan-repadd-{}", new_id()));
        let mut plan = create(
            "goal".to_string(),
            Vec::new(),
            Vec::new(),
            vec![NewStep {
                title: "work".into(),
                refs: Vec::new(),
            }],
            1000,
            &Limits::default(),
        )
        .unwrap();
        plan.applied_event = Some("ra:0".to_string());
        store(&dir, &plan).unwrap();
        let mut journal = crate::agent::journal::Journal::open(&dir, "ra").unwrap();
        journal
            .append(
                "plan",
                serde_json::json!({
                    "op": "add_acceptance",
                    "items": ["cmd: exit 3", "manual: eyeball it"],
                    "plan_id": plan.id, "by": "model", "ok": true,
                    "new_baselines": [
                        {"at": "t", "exit": 3, "check_definition_hash": "h",
                         "output_hash": "o", "head": "", "state_digest": "s"},
                        null
                    ],
                    "new_snapshots": [null, null],
                    "new_shapes": [null, null],
                    "new_inputs": [[], []],
                }),
            )
            .unwrap();
        // the stored file predates the intent (cursor ra:0, intent at seq
        // 1): replay must append the items AND restore the riding proof
        let report = replay(&dir).unwrap();
        assert!(report.ops_applied >= 1, "{report:?}");
        let rebuilt = open(&dir, &plan.id).unwrap();
        assert_eq!(rebuilt.acceptance.len(), 2);
        assert_eq!(rebuilt.acceptance[0].text, "cmd: exit 3");
        let baseline = rebuilt.acceptance[0].baseline.as_ref().expect("proof restored");
        assert_eq!(baseline.exit, 3);
        assert!(rebuilt.acceptance[1].baseline.is_none(), "manual proves nothing");
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
            vec!["cmd: check-docs".to_string()],
            vec![NewStep {
                title: "verify docs".into(),
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

    #[test]
    fn constraints_classify_by_prefix() {
        assert!(matches!(
            classify_constraint("forbid-import: btree"),
            ConstraintKind::ForbidImport("btree")
        ));
        assert!(matches!(
            classify_constraint("forbid-cmd: rm -rf"),
            ConstraintKind::ForbidCmd("rm -rf")
        ));
        assert!(matches!(
            classify_constraint("ast: foo($X)"),
            ConstraintKind::Ast("foo($X)")
        ));
        match classify_constraint("path: src/a, src/b") {
            ConstraintKind::Path(roots) => assert_eq!(roots, vec!["src/a", "src/b"]),
            other => panic!("path: must parse roots, got {other:?}"),
        }
        assert!(matches!(
            classify_constraint("keep the format"),
            ConstraintKind::Plain(_)
        ));
        // empty payloads stay classified (evaluators treat them as vacuous)
        assert!(matches!(
            classify_constraint("path:   "),
            ConstraintKind::Path(roots) if roots.is_empty()
        ));
    }

    #[test]
    fn waive_constraint_needs_bounds_and_renders() {
        let mut plan = new_plan();
        plan.constraints = vec!["forbid-import: btree".to_string()];
        let err = waive_constraint(&mut plan, 3, "x").unwrap_err();
        assert_eq!(err.code, "unknown_constraint");
        waive_constraint(&mut plan, 0, "legacy use, tracked").unwrap();
        assert_eq!(
            waived_constraint_indices(&plan),
            vec![0],
            "evaluators skip waived indices"
        );
        // idempotent: the first reason stands
        waive_constraint(&mut plan, 0, "other").unwrap();
        assert_eq!(plan.waived_constraints.len(), 1);
        assert!(render(&plan).contains("[waived]"));
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

    /// Same stripping rule for `snapshot:` items.
    fn snapshot_for(item_text: &str) -> Snapshot {
        let command = item_text
            .strip_prefix("snapshot:")
            .unwrap_or(item_text)
            .trim();
        Snapshot {
            at: now(),
            exit: Some(0),
            check_definition_hash: check_definition_hash(command),
            output_hash: "outhash".to_string(),
            head: "frozen output".to_string(),
            state_digest: "digest".to_string(),
        }
    }

    #[test]
    fn snapshot_freezes_only_against_the_check_it_was_taken_on() {
        let mut plan = create(
            "freeze the output".to_string(),
            Vec::new(),
            vec!["snapshot: mycli --version".to_string()],
            vec![NewStep {
                title: "do the work".to_string(),
                refs: Vec::new(),
            }],
            0,
            &Limits::default(),
        )
        .unwrap();
        assert!(!snapshot_current(&plan.acceptance[0]), "nothing frozen yet");
        assert!(render(&plan).contains("[no snapshot]"));
        set_snapshots(&mut plan, vec![Some(snapshot_for("snapshot: mycli --version"))]);
        assert!(snapshot_current(&plan.acceptance[0]));
        assert!(!render(&plan).contains("[no snapshot]"));
        // rewriting the command makes a new check: the old output stops
        // applying to it, the same freeze logic as baselines
        plan.acceptance[0].text = "snapshot: mycli --help".to_string();
        assert!(!snapshot_current(&plan.acceptance[0]));
        assert!(render(&plan).contains("[no snapshot]"));
    }

    #[test]
    fn snapshot_never_applies_to_other_kinds() {
        let mut plan = new_plan(); // acceptance[0] is "cmd: cargo test"
        set_snapshots(&mut plan, vec![Some(snapshot_for("cmd: cargo test"))]);
        assert!(!snapshot_current(&plan.acceptance[0]));
        assert!(!proven_failing(&plan.acceptance[0]));
    }

    #[test]
    fn acceptance_kinds_parse_by_prefix() {
        let kinds = [
            ("cmd: cargo test", "cmd"),
            ("snapshot: mycli --version", "snapshot"),
            ("differential: mycli render fix", "differential"),
            ("signatures: src/a.rs, src/b.rs", "signatures"),
            ("manual: eyeball it", "manual"),
            ("the page renders", "text"),
        ];
        for (text, want) in kinds {
            let item = Acceptance {
                text: text.to_string(),
                status: AcceptanceStatus::Pending,
                evidence: Vec::new(),
                validation: Validation::default(),
                baseline: None,
                snapshot: None,
                shape: None,
                inputs: Vec::new(),
                by: None,
                reason: None,
            };
            let got = match item.kind() {
                AcceptanceKind::Command(_) => "cmd",
                AcceptanceKind::Snapshot(_) => "snapshot",
                AcceptanceKind::Differential(_) => "differential",
                AcceptanceKind::Signatures(_) => "signatures",
                AcceptanceKind::Manual(_) => "manual",
                AcceptanceKind::Text(_) => "text",
            };
            assert_eq!(got, want, "{text}");
        }
    }

    #[test]
    fn differential_shares_the_freeze_with_the_inverted_verdict() {
        let mut plan = create(
            "move the output".to_string(),
            Vec::new(),
            vec!["differential: mycli render fix".to_string()],
            vec![NewStep {
                title: "do the work".to_string(),
                refs: Vec::new(),
            }],
            0,
            &Limits::default(),
        )
        .unwrap();
        assert!(!differential_current(&plan.acceptance[0]));
        assert!(!snapshot_current(&plan.acceptance[0]));
        assert!(render(&plan).contains("[no differential]"));
        // the freeze record is the same shape rung 4 uses
        set_snapshots(
            &mut plan,
            vec![Some(snapshot_for("snapshot: mycli render fix"))],
        );
        assert!(differential_current(&plan.acceptance[0]));
        assert!(!render(&plan).contains("[no differential]"));
        plan.acceptance[0].text = "differential: mycli render other".to_string();
        assert!(!differential_current(&plan.acceptance[0]));
    }

    fn shape_for(paths: &str) -> ShapeFreeze {
        ShapeFreeze {
            at: now(),
            check_definition_hash: check_definition_hash(paths),
            files: vec![ShapeFile {
                path: "src/a.rs".to_string(),
                parser: "ts:rust".to_string(),
                shape_hash: "shapehash".to_string(),
                items: 2,
            }],
        }
    }

    #[test]
    fn signatures_freeze_only_against_the_named_file_set() {
        let mut plan = create(
            "hold the shape".to_string(),
            Vec::new(),
            vec!["signatures: src/a.rs".to_string()],
            vec![NewStep {
                title: "do the work".to_string(),
                refs: Vec::new(),
            }],
            0,
            &Limits::default(),
        )
        .unwrap();
        assert!(!signatures_current(&plan.acceptance[0]));
        assert!(render(&plan).contains("[no signatures]"));
        set_shapes(&mut plan, vec![Some(shape_for("src/a.rs"))]);
        assert!(signatures_current(&plan.acceptance[0]));
        assert!(!render(&plan).contains("[no signatures]"));
        // renaming the file set makes a new check
        plan.acceptance[0].text = "signatures: src/b.rs".to_string();
        assert!(!signatures_current(&plan.acceptance[0]));
    }

    fn frozen_tree() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sqwai-freeze-{}", new_id()));
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("tests/auth.rs"), "fn t() {}\n").unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("target/cached.rlib"), "blob").unwrap();
        dir
    }

    #[test]
    fn freeze_check_inputs_covers_test_layout_not_build_outputs() {
        let dir = frozen_tree();
        let inputs = freeze_check_inputs(&dir);
        assert_eq!(
            inputs.iter().map(|i| i.path.clone()).collect::<Vec<_>>(),
            vec!["tests/auth.rs".to_string()],
            "conventional test layout in, src/ and target/ out: {inputs:?}"
        );
        // sorted and hashed
        assert!(!inputs[0].hash.is_empty());
        assert!(changed_check_inputs(&dir, &inputs).is_empty());

        // edit and deletion both read as changed
        std::fs::write(dir.join("tests/auth.rs"), "fn t() {}\nfn u() {}\n").unwrap();
        assert_eq!(
            changed_check_inputs(&dir, &inputs),
            vec!["tests/auth.rs".to_string()]
        );
        std::fs::remove_file(dir.join("tests/auth.rs")).unwrap();
        assert_eq!(
            changed_check_inputs(&dir, &inputs),
            vec!["tests/auth.rs".to_string()]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn frozen_input_paths_skip_waived_items() {
        let mut plan = create(
            "frozen".to_string(),
            Vec::new(),
            vec!["cmd: cargo test".to_string(), "cmd: cargo clippy".to_string()],
            vec![NewStep {
                title: "do the work".to_string(),
                refs: Vec::new(),
            }],
            0,
            &Limits::default(),
        )
        .unwrap();
        let inputs = vec![CheckInput {
            path: "tests/a.rs".to_string(),
            hash: "h".to_string(),
        }];
        set_inputs(&mut plan, vec![inputs.clone(), inputs]);
        assert_eq!(frozen_input_paths(&plan).len(), 2);
        plan.acceptance[0].status = AcceptanceStatus::Waived;
        // waiving takes responsibility: the item's inputs unfreeze
        assert_eq!(
            frozen_input_paths(&plan),
            vec!["tests/a.rs".to_string()]
        );
    }

    fn rung_of(text: &str) -> Option<Rung> {
        ladder_rung(&Acceptance {
            text: text.to_string(),
            status: AcceptanceStatus::Pending,
            evidence: Vec::new(),
            validation: Validation::default(),
            baseline: None,
            snapshot: None,
            shape: None,
            inputs: Vec::new(),
            by: None,
            reason: None,
        })
    }

    #[test]
    fn ladder_classifies_kinds_exactly_and_commands_by_heuristic() {
        // kinds that name their rung map exactly
        assert_eq!(rung_of("differential: x"), Some(Rung::Differential));
        assert_eq!(rung_of("snapshot: x"), Some(Rung::Snapshot));
        assert_eq!(rung_of("signatures: x"), Some(Rung::Structural));
        // cmd: reads test-shaped, build-shaped, or fixture-shaped
        assert_eq!(rung_of("cmd: cargo test"), Some(Rung::Test));
        assert_eq!(rung_of("cmd: pytest -x"), Some(Rung::Test));
        assert_eq!(rung_of("cmd: go test ./..."), Some(Rung::Test));
        assert_eq!(rung_of("cmd: cargo check"), Some(Rung::Build));
        assert_eq!(rung_of("cmd: tsc --noEmit"), Some(Rung::Build));
        assert_eq!(rung_of("cmd: npm run build"), Some(Rung::Build));
        assert_eq!(rung_of("cmd: ./run-fixture.sh"), Some(Rung::Fixture));
        // manual and free text engage no rung
        assert_eq!(rung_of("manual: eyeball it"), None);
        assert_eq!(rung_of("the page renders"), None);
    }

    #[test]
    fn ladder_walk_stops_at_the_highest_trust_rung() {        let mut plan = create(
            "walk the ladder".to_string(),
            Vec::new(),
            vec![
                "manual: eyeball it".to_string(),
                "cmd: ./run-fixture.sh".to_string(),
                "snapshot: mycli --version".to_string(),
            ],
            vec![NewStep {
                title: "do the work".to_string(),
                refs: Vec::new(),
            }],
            0,
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(ladder_top(&plan), Some(Rung::Snapshot));
        assert!(ladder_note(&plan).contains("rung 4 snapshot"));
        assert!(render(&plan).contains("[rung 4 snapshot]"));
        assert!(render(&plan).contains("[rung 8 fixture]"));
        // manual only: no rung to stand on
        plan.acceptance.retain(|item| ladder_rung(item).is_none());
        assert_eq!(ladder_top(&plan), None);
        assert!(ladder_note(&plan).contains("no executable rung"));
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
    fn baseline_never_proves_a_manual_item() {        let mut plan = create(
            "render the page".to_string(),
            Vec::new(),
            vec![
                "manual: eyeball it".to_string(),
                "manual: read it aloud".to_string(),
            ],
            vec![NewStep {
                title: "do the work".to_string(),
                refs: Vec::new(),
            }],
            0,
            &Limits::default(),
        )
        .unwrap();
        // even handed the same shape of proof, manual items can never be
        // settled by a host run: only a command is runnable
        set_baselines(
            &mut plan,
            vec![
                Some(baseline_for("manual: eyeball it")),
                Some(baseline_for("manual: read it aloud")),
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
    fn apply_flaky_needs_a_pass_and_is_idempotent() {
        let mut plan = new_plan();
        // a first red run is a failure, not a disagreement
        assert!(!apply_flaky(&mut plan, 0));
        assert_eq!(
            plan.acceptance[0].validation.status,
            ValidationStatus::Pending
        );
        plan.acceptance[0].status = AcceptanceStatus::Passed;
        assert!(apply_flaky(&mut plan, 0));
        assert_eq!(
            plan.acceptance[0].validation.status,
            ValidationStatus::Unknown
        );
        // replay converges: marking twice changes nothing
        assert!(!apply_flaky(&mut plan, 0));
        assert!(!apply_flaky(&mut plan, 9));
    }

    #[test]
    fn unknown_validation_blocks_complete_until_waived() {
        let mut plan = new_plan();
        close_steps(&mut plan);
        plan.acceptance[0].status = AcceptanceStatus::Passed;
        assert!(apply_flaky(&mut plan, 0));
        let err = apply(&mut plan, Op::Complete, &Limits::default(), None).unwrap_err();
        assert_eq!(err.code, "acceptance_pending");
        // waiver is the way out of the third state
        waive(&mut plan, 0, "flaky upstream, tracked separately").unwrap();
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
                refs: vec![],
            },
            NewStep {
                title: "part b".into(),
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
                refs: vec![],
            },
            NewStep {
                title: "part b".into(),
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
