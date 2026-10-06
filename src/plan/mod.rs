//! Structured plan (DESIGN §2.1).
//!
//! A plan is a host-owned document: the model reaches it only through the
//! `plan` tool operations validated here. The model can never write `goal`,
//! `constraints`, `evidence` or `folded` directly. `criteria` are the agent's
//! own plain-text done-notes — the host stores and renders them, but runs and
//! certifies nothing.
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
pub(crate) mod store;
mod verify;
pub use ops::{
    Applied, Limits, NewStep, Op, PlanDraftArgs, Rejection, abandon, apply, create, reset_discards,
    validate_proposal_invariants, validate_surrender_reason,
};
pub(crate) use ops::{add_criteria, step_diff};
pub use render::{render, render_goal, render_status, set_goal};
pub use store::{
    StepContext, commit, list, list_active, open, open_active, open_active_for_session,
    read_plan_file, replay, store,
};
pub(crate) use verify::{complete, next_id};

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
    /// Done-criteria the agent writes for itself: plain-text notes on what
    /// must be true when the work is done. They survive compaction so the
    /// post-compaction agent resumes against a stated target, and the host
    /// never gates on them — no kinds, no prefixes, no settle machinery.
    /// Replaces the old `acceptance` items and `checklist` (migrated on
    /// load; see `store::parse_plan`).
    #[serde(default)]
    pub criteria: Vec<String>,
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
        assert!(
            goal_before.contains("persist the plan on disk"),
            "{goal_before}"
        );
        assert!(goal_before.contains("no new dependencies"), "{goal_before}");
        assert!(!goal_before.contains("add the model"), "{goal_before}");
        assert!(!goal_before.contains("active"), "{goal_before}");

        let status_before = render_status(&plan);
        assert!(status_before.contains("active"), "{status_before}");
        assert!(status_before.contains("add the model"), "{status_before}");

        plan.steps[0].status = StepStatus::Done;
        plan.steps[1].status = StepStatus::InProgress;
        assert_eq!(
            render_goal(&plan),
            goal_before,
            "step moves re-key nothing cached"
        );
        assert_ne!(render_status(&plan), status_before, "step moves surface");

        // the full render still carries both halves
        let full = render(&plan);
        assert!(full.contains("persist the plan on disk"), "{full}");
        assert!(full.contains("[x] 1 add the model"), "{full}");
    }

    fn new_plan() -> Plan {
        create(
            "persist the plan on disk".to_string(),
            vec!["no new dependencies".to_string()],
            vec!["cargo test must pass".to_string()],
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
                    summary: "model added".into()
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
                    reason: "goal targets removed feature X, steps assume the deleted API"
                        .to_string(),
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
    fn complete_needs_only_closed_steps() {
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
                },
                &Limits::default(),
                None,
            )
            .unwrap();
        }
        // the acceptance/validation machinery is retired: closing every step
        // is enough to complete — nothing left for the host to certify.
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
        let active = new_plan(); // constraint "no new dependencies", criterion "cargo test must pass"
        let mut draft = PlanDraftArgs {
            goal: active.goal.text.clone(),
            constraints: vec![], // dropped!
            criteria: vec!["cargo test must pass".into()],
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

        // 3. Dropped criteria rejected
        draft.criteria = vec![];
        let res = validate_proposal_invariants(Some(&active), &draft);
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().code, "dropped_criteria");

        // 4. Changing goal allows new constraints and criteria
        draft.goal = "A completely different goal".into();
        draft.constraints = vec!["new constraint".into()];
        draft.criteria = vec!["new note".into()];
        assert!(validate_proposal_invariants(Some(&active), &draft).is_ok());
    }

    #[test]
    fn complete_rejects_reopened_steps() {
        let mut plan = new_plan();
        plan.steps[0].status = StepStatus::Done;
        plan.steps[1].status = StepStatus::Reopened;
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
                "goal": "g", "constraints": [], "criteria": ["note"],
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
                "goal": "g", "constraints": [], "criteria": ["note"],
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

    /// Late-added criteria survive a crash rebuild: notes re-append from the
    /// journaled intent instead of being lost when the store lagged behind.
    #[test]
    fn replay_restores_added_criteria() {
        let dir = std::env::temp_dir().join(format!("sqwai-plan-repadd-{}", new_id()));
        let mut plan = create(
            "goal".to_string(),
            Vec::new(),
            vec!["first note".to_string()],
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
                    "op": "add_criteria",
                    "criteria": ["exit 3 first", "eyeball it"],
                    "plan_id": plan.id, "by": "model", "ok": true,
                }),
            )
            .unwrap();
        // the stored file predates the intent (cursor ra:0, intent at seq
        // 1): replay re-appends the notes from the journaled intent.
        let report = replay(&dir).unwrap();
        assert!(report.ops_applied >= 1, "{report:?}");
        let rebuilt = open(&dir, &plan.id).unwrap();
        assert_eq!(rebuilt.criteria.len(), 3);
        assert_eq!(rebuilt.criteria[0], "first note");
        assert_eq!(rebuilt.criteria[1], "exit 3 first");
        assert_eq!(rebuilt.criteria[2], "eyeball it");
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
        // a legacy v1 file still loads: the retired `acceptance` objects are
        // ignored (no plain-text criteria to migrate from an object shape),
        // so criteria defaults empty while the step's refs and evidence parse.
        assert!(plan.criteria.is_empty());
        assert_eq!(plan.applied_event, None);
        let step = &plan.steps[0];
        assert_eq!(step.step_epoch, 0);
        assert_eq!(step.evidence.len(), 2);
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
    fn reopen_bumps_epoch_and_clears_evidence() {
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
            },
            &Limits::default(),
            None,
        )
        .unwrap();
        assert_eq!(plan.step("1").unwrap().step_epoch, 0);

        reopen_for_undo(&mut plan, "1", "reopened by undo").unwrap();

        let step = plan.step("1").unwrap();
        assert_eq!(step.status, StepStatus::Reopened);
        assert_eq!(step.step_epoch, 1);
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
