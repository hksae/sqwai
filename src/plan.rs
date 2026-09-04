//! Structured plan (DESIGN §2.1).
//!
//! A plan is a host-owned document: the model reaches it only through the
//! `plan` tool operations validated here. The model can never write `goal`,
//! `constraints`, `acceptance[].status`, `evidence` or `folded` directly.
//!

use anyhow::{Context, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

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
    pub fn is_closed(self) -> bool {
        matches!(self, Self::Done | Self::Cancelled)
    }

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
    Verified,
    Waived,
}

impl AcceptanceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Verified => "verified",
            Self::Waived => "waived",
        }
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
    pub evidence: Vec<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
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
    pub evidence: Vec<u64>,
    /// Stable graph keys the step intends to touch (§2.4.3).
    #[serde(default)]
    pub refs: Vec<String>,
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
    pub evidence: Vec<u64>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    #[serde(default)]
    pub sessions: Vec<String>,
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
    let dir = plans_dir(root);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(plan) = serde_json::from_str::<Plan>(&text) {
            if plan.status == PlanStatus::Active {
                return Ok(Some(plan));
            }
        }
    }
    Ok(None)
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

// ---------------------------------------------------------------- operations

#[derive(Debug, Clone, Deserialize)]
pub struct NewStep {
    pub title: String,
    #[serde(default)]
    pub kind: Option<StepKind>,
    #[serde(default)]
    pub refs: Vec<String>,
}

/// One operation per call (§2.1.3).
#[derive(Debug, Clone, Deserialize)]
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
        id: String,
        reason: String,
    },
    Add {
        #[serde(default)]
        after: Option<String>,
        title: String,
        #[serde(default)]
        kind: Option<StepKind>,
        #[serde(default)]
        refs: Vec<String>,
    },
    Split {
        id: String,
        into: Vec<NewStep>,
    },
    Verify {
        acceptance: usize,
        #[serde(default)]
        evidence: Vec<u64>,
    },
    Complete,
    ProposeGoalRevision {
        goal: String,
        reason: String,
    },
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
pub enum Applied {
    /// `create` result — the caller must persist the plan.
    Created(Plan),
    Updated {
        message: String,
    },
    /// Needs user confirmation before it takes effect (§2.1.6).
    Proposed {
        goal: String,
        reason: String,
    },
    Shown {
        text: String,
    },
    Completed,
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
        forked_from: None,
        sessions: Vec::new(),
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
pub fn apply(plan: &mut Plan, op: Op, limits: &Limits) -> Result<Applied, Rejection> {
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
        Op::Start { id, confirm } => start(plan, &id, confirm),
        Op::Finish {
            id,
            summary,
            evidence,
        } => finish(plan, &id, summary, evidence),
        Op::Block { id, reason } => block(plan, &id, reason),
        Op::Unblock { id } => unblock(plan, &id),
        Op::Cancel { id, reason } => cancel(plan, &id, reason),
        Op::Add {
            after,
            title,
            kind,
            refs,
        } => add(plan, after.as_deref(), title, kind, refs, limits),
        Op::Split { id, into } => split(plan, &id, into, limits),
        Op::Verify {
            acceptance,
            evidence,
        } => verify(plan, acceptance, evidence),
        Op::Complete => complete(plan),
        Op::ProposeGoalRevision { goal, reason } => propose_goal_revision(plan, goal, reason),
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

fn start(plan: &mut Plan, id: &str, confirm: Option<bool>) -> Result<Applied, Rejection> {
    let Some((status, stale)) = step_status(plan, id) else {
        return unknown_step(plan, id);
    };
    if status != StepStatus::Pending {
        return reject(
            plan,
            "step_not_pending",
            format!("step {id} is {}", status.as_str()),
            "only a pending step can be started",
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
    evidence: Vec<u64>,
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
    if evidence.is_empty() {
        return reject(
            plan,
            "no_evidence",
            format!("step {id} has no journal evidence"),
            "provide evidence sequence numbers from tool results or file diffs",
        );
    }
    let step = plan.step_mut(id).expect("checked above");
    step.evidence = evidence;
    step.status = StepStatus::Done;
    step.finished = Some(now());
    step.summary = Some(summary);
    accept(plan, format!("step {id} done"))
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

fn cancel(plan: &mut Plan, id: &str, reason: String) -> Result<Applied, Rejection> {
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
    if reason.trim().is_empty() {
        return reject(
            plan,
            "empty_reason",
            format!("cancelling step {id} needs a reason"),
            "say why it will not be done",
        );
    }
    let step = plan.step_mut(id).expect("checked above");
    step.status = StepStatus::Cancelled;
    step.reason = Some(reason);
    accept(plan, format!("step {id} cancelled"))
}

fn add(
    plan: &mut Plan,
    after: Option<&str>,
    title: String,
    kind: Option<StepKind>,
    refs: Vec<String>,
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
            stale_goal: None,
        })
        .collect();
    let ids: Vec<String> = replacements.iter().map(|s| s.id.clone()).collect();
    plan.steps.splice(index..=index, replacements);
    accept(plan, format!("step {id} split into {}", ids.join(", ")))
}

fn verify(plan: &mut Plan, index: usize, evidence: Vec<u64>) -> Result<Applied, Rejection> {
    if index >= plan.acceptance.len() {
        return reject(
            plan,
            "unknown_acceptance",
            format!("no acceptance item {index}"),
            "call plan show to see the acceptance list",
        );
    }
    // Evidence-seq validation needs the journal (F2) — checked there.
    let item = &mut plan.acceptance[index];
    item.status = AcceptanceStatus::Verified;
    item.evidence = evidence;
    accept(plan, format!("acceptance {index} verified"))
}

fn complete(plan: &mut Plan) -> Result<Applied, Rejection> {
    let pending: Vec<String> = plan
        .steps
        .iter()
        .filter(|s| s.status == StepStatus::Pending || s.status == StepStatus::InProgress)
        .map(|s| s.id.clone())
        .collect();
    if !pending.is_empty() {
        return reject(
            plan,
            "steps_open",
            format!("steps still open: {}", pending.join(", ")),
            "finish, cancel or block them first",
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
            "verify them, or have the user waive them with /plan waive",
        );
    }
    plan.status = PlanStatus::Completed;
    plan.revision += 1;
    plan.rejections_in_a_row = 0;
    Ok(Applied::Completed)
}

fn propose_goal_revision(
    plan: &mut Plan,
    goal: String,
    reason: String,
) -> Result<Applied, Rejection> {
    if goal.trim().is_empty() {
        return reject(
            plan,
            "empty_goal",
            "a goal revision needs the new goal text",
            "state what must be true when the work is done",
        );
    }
    if goal.trim() == plan.goal.text.trim() {
        return reject(
            plan,
            "identical_goal",
            "the proposed goal is identical to the current one",
            "change the goal text, or leave it alone",
        );
    }
    if reason.trim().is_empty() {
        return reject(
            plan,
            "empty_reason",
            "a goal revision needs a reason",
            "say why the goal changed",
        );
    }
    plan.rejections_in_a_row = 0;
    // Applied by the host after the user confirms (§2.1.6).
    Ok(Applied::Proposed { goal, reason })
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
    fn start_then_finish() {
        let mut plan = new_plan();
        assert!(matches!(
            apply(
                &mut plan,
                Op::Start {
                    id: "1".into(),
                    confirm: None
                },
                &Limits::default()
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
                &Limits::default()
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
        )
        .unwrap_err();
        assert_eq!(err.code, "step_not_in_progress");
        assert_eq!(plan.rejections_in_a_row, 1);
    }

    #[test]
    fn complete_requires_all_steps_closed() {
        let mut plan = new_plan();
        let err = apply(&mut plan, Op::Complete, &Limits::default()).unwrap_err();
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
            )
            .unwrap();
        }
        let err = apply(&mut plan, Op::Complete, &Limits::default()).unwrap_err();
        assert_eq!(err.code, "acceptance_pending");
        waive(&mut plan, 0, "manual check").unwrap();
        assert!(matches!(
            apply(&mut plan, Op::Complete, &Limits::default()),
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
                &Limits::default()
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
}
