//! Structured plan.
//!
//! A plan is a host-owned document: the model reaches it only through the
//! `plan` tool operations validated here. The model can never write `goal`,
//! `constraints` or `evidence` directly. `criteria` are the agent's
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
pub use ops::{Applied, Limits, NewStep, Op, Rejection, apply, create};
pub(crate) use ops::auto_advance;
pub use render::{render, render_goal, render_status};
#[cfg(test)]
pub use store::open_active;
pub use store::{
    StepContext, commit, list, list_active, open, open_active_for_session, read_plan_file, replay,
    store,
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

/// Default ceiling for the number of steps.
pub const MAX_STEPS_DEFAULT: usize = 24;

/// Current plan file schema version, written by `create`. Files with a
/// newer version come from a newer binary and must not be read as valid.
pub const PLAN_VERSION: u32 = 3;

// ---------------------------------------------------------------- model

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Active,
    Completed,
    #[serde(alias = "blocked")]
    Abandoned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    InProgress,
    Done,
    #[serde(alias = "blocked")]
    Cancelled,
    Reopened,
}

impl StepStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
            Self::Reopened => "reopened",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub text: String,
    pub source: String,
    pub created: String,
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
    /// Journal seq values. Written by the host only.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
    /// Bumped by every host-only reopen; subagent evidence from an older
    /// epoch does not count.
    #[serde(default)]
    pub step_epoch: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub version: u32,
    pub id: String,
    pub status: PlanStatus,
    pub created: String,
    #[serde(default)]
    pub sessions: Vec<String>,
    /// Per-session replay cursors (`session -> last applied journal seq`).
    /// Each session advances only its own entry; sessions with no entry
    /// replay nothing. The plan is a projection replayed from here on load.
    #[serde(default)]
    pub applied_events: std::collections::BTreeMap<String, u64>,
    pub goal: Goal,
    #[serde(default)]
    pub constraints: Vec<String>,
    /// Done-criteria the agent writes for itself: plain-text notes on what
    /// must be true when the work is done. They survive compaction so the
    /// post-compaction agent resumes against a stated target, and the host
    /// never gates on them — no kinds, no prefixes, no settle machinery.
    #[serde(default)]
    pub criteria: Vec<String>,
    #[serde(default)]
    pub steps: Vec<Step>,
    pub revision: u64,
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
/// Conservative rule: ANY reverted part of the step's result reopens
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
