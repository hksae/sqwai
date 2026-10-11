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
pub use ops::{Applied, Limits, NewStep, Op, Rejection, abandon, apply, create};
pub(crate) use ops::{add_criteria, step_diff};
pub use render::{render, render_goal, render_status, set_goal};
pub use store::{
    StepContext, commit, list, list_active, open, open_active, open_active_for_session,
    read_plan_file, replay, store,
};

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
mod tests;
