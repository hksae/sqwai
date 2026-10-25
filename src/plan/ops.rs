use super::{Goal, MAX_STEPS_DEFAULT, PLAN_VERSION, Plan, PlanStatus, Step, StepStatus, new_id, now};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------- operations

#[derive(Debug, Clone, Serialize)]
pub struct NewStep {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
}

impl<'de> Deserialize<'de> for NewStep {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let val = Value::deserialize(deserializer)?;
        if let Some(s) = val.as_str() {
            return Ok(NewStep {
                title: s.to_string(),
                check: None,
            });
        }
        if let Some(obj) = val.as_object() {
            for key in ["title", "step", "description", "name", "text"] {
                if let Some(s) = obj.get(key).and_then(Value::as_str) {
                    return Ok(NewStep {
                        title: s.to_string(),
                        check: obj
                            .get("check")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    });
                }
            }
        }
        Err(serde::de::Error::custom(
            "expected a step string or an object with a `title` field",
        ))
    }
}

fn normalize_check(check: Option<String>) -> Option<String> {
    check
        .map(|check| check.trim().to_string())
        .filter(|check| !check.is_empty())
}

/// Plan notes are plain text. The model sometimes emits XML-ish wrappers
/// (`note</criteria>`); strip tag-shaped runs but leave comparisons such as
/// `latency < 100 ms` intact.
pub(crate) fn strip_tags(text: &str) -> String {
    static TAGS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"</?[A-Za-z][^<>]*>").expect("plan tag pattern")
    });
    TAGS.replace_all(text, "").trim().to_string()
}

/// One operation per call. `Serialize` is for the journal intent
/// record: every accepted op is journaled with its full args before the
/// plan file is stored, so a crash between the two heals by replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Create {
        goal: String,
        #[serde(default)]
        constraints: Vec<String>,
        /// Done-criteria notes the agent writes for itself (plain text).
        /// Required non-empty — the one create-time check, a form
        /// completeness gate, not a workflow gate.
        #[serde(default, alias = "acceptance")]
        criteria: Vec<String>,
        #[serde(default)]
        steps: Vec<NewStep>,
    },
    Finish {
        #[serde(default)]
        id: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        ids: Vec<String>,
        summary: String,
    },
    Add {
        #[serde(default)]
        after: Option<String>,
        title: String,
        #[serde(default)]
        check: Option<String>,
    },
    Cancel {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        reason: String,
    },
    /// Abandon the whole plan when the direction has changed or the
    /// specification is contradictory. `reason` must explain why (empty/thin
    /// reasons refused). The old plan stays on disk as `Abandoned` (history is
    /// never rewritten); a replacement, if any, goes through a fresh `create`.
    #[serde(alias = "propose_reset", alias = "block_plan")]
    Abandon {
        #[serde(default)]
        reason: String,
    },
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
pub enum Applied {
    Updated { message: String },
    Completed { message: String },
}

/// Host-only: retire the active plan. The reason travels in the journal; the
/// file keeps no history of why it was abandoned (history is never rewritten).
pub fn abandon(plan: &mut Plan) {
    plan.status = PlanStatus::Abandoned;
    plan.revision += 1;
}

/// Reason gate for `Abandon`: must explain what is wrong (spec conflict or
/// direction change), not gesture at effort. Empty and one-word reasons are
/// refused with guidance.
pub fn validate_surrender_reason(reason: &str, what: &str) -> Result<String, Rejection> {
    let quoted = reason.trim().to_string();
    if quoted.is_empty() {
        return Err(Rejection::new(
            "empty_reason",
            format!("{what} needs a reason"),
            "cite what changed or contradicts what: the requirement, step, or spec conflict"
                .to_string(),
        ));
    }
    if quoted.split_whitespace().count() < 3 {
        return Err(Rejection::new(
            "thin_reason",
            format!("{what} reason is too thin to judge: {quoted}"),
            "quote the defect itself — which requirement, step, or criterion is wrong and why"
                .to_string(),
        ));
    }
    Ok(quoted)
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

pub(crate) fn next_id(plan: &Plan) -> Result<String, Rejection> {
    let mut max = 0usize;
    for s in &plan.steps {
        let n = s.id.parse::<usize>().map_err(|_| {
            Rejection::new(
                "bad_step_id",
                format!("step id {} is not a number", s.id),
                "step ids are numbers; fix the plan file before adding steps",
            )
        })?;
        max = max.max(n);
    }
    Ok((max + 1).to_string())
}

/// Promote the first `Pending` or `Reopened` step to `InProgress` whenever no
/// step is currently running. Returns the ID of the newly started step, if any.
pub(crate) fn auto_advance(plan: &mut Plan) -> Option<String> {
    if plan.steps.iter().any(|s| s.status == StepStatus::InProgress) {
        return None;
    }
    if let Some(next) = plan
        .steps
        .iter_mut()
        .find(|s| matches!(s.status, StepStatus::Pending | StepStatus::Reopened))
    {
        next.status = StepStatus::InProgress;
        if next.started.is_none() {
            next.started = Some(now());
        }
        return Some(next.id.clone());
    }
    None
}

/// Build a new plan and immediately start step 1 (`InProgress`). Rejects an
/// empty goal, empty criteria, zero steps, empty step titles, and over-long
/// plans.
pub fn create(
    goal: String,
    constraints: Vec<String>,
    criteria: Vec<String>,
    steps: Vec<NewStep>,
    limits: &Limits,
) -> Result<Plan, Rejection> {
    if goal.trim().is_empty() {
        return Err(Rejection::new(
            "empty_goal",
            "a plan needs a goal",
            "state what must be true when the work is done",
        ));
    }
    let constraints: Vec<String> = constraints
        .into_iter()
        .map(|text| strip_tags(&text))
        .filter(|text| !text.is_empty())
        .collect();
    let criteria: Vec<String> = criteria
        .into_iter()
        .map(|text| strip_tags(&text))
        .filter(|text| !text.is_empty())
        .collect();
    if criteria.is_empty() {
        return Err(Rejection::new(
            "empty_criteria",
            "a plan needs at least one done-criterion",
            "add `criteria`: plain notes on what must be true when the work is done \
             (e.g. the command that proves it, or the behaviour to check by hand)",
        ));
    }
    if steps.is_empty() {
        return Err(Rejection::new(
            "no_steps",
            "a plan needs at least one step",
            "break the work into 3-6 steps",
        ));
    }
    if steps.iter().any(|s| s.title.trim().is_empty()) {
        return Err(Rejection::new(
            "empty_title",
            "every step in a plan needs a non-empty title",
            "give each step a clear one-line description",
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
            "merge steps, or ask the user to raise [plan].max_steps in the config",
        ));
    }

    let ts = now();
    let mut dedup_criteria: Vec<String> = Vec::new();
    for text in criteria {
        if !dedup_criteria
            .iter()
            .any(|c| c.eq_ignore_ascii_case(&text))
        {
            dedup_criteria.push(text);
        }
    }
    let plan = Plan {
        version: PLAN_VERSION,
        id: new_id(),
        status: PlanStatus::Active,
        created: ts.clone(),
        sessions: Vec::new(),
        applied_events: Default::default(),
        goal: Goal {
            text: goal.trim().to_string(),
            source: "user".to_string(),
            created: ts.clone(),
        },
        constraints,
        criteria: dedup_criteria,
        steps: steps
            .into_iter()
            .enumerate()
            .map(|(i, s)| Step {
                id: (i + 1).to_string(),
                title: s.title.trim().to_string(),
                check: normalize_check(s.check),
                status: if i == 0 {
                    StepStatus::InProgress
                } else {
                    StepStatus::Pending
                },
                started: if i == 0 { Some(ts.clone()) } else { None },
                finished: None,
                summary: None,
                reason: None,
                evidence: Vec::new(),
                step_epoch: 0,
            })
            .collect(),
        revision: 0,
    };
    Ok(plan)
}

/// Apply one operation to an active plan.
pub fn apply(plan: &mut Plan, op: Op, limits: &Limits) -> Result<Applied, Rejection> {
    if plan.status != PlanStatus::Active {
        return reject(
            plan,
            "plan_closed",
            format!(
                "plan {} is {}",
                plan.id,
                match plan.status {
                    PlanStatus::Completed => "completed",
                    PlanStatus::Abandoned => "abandoned",
                    PlanStatus::Active => "active",
                }
            ),
            "closed plans are read-only; start a new plan with op=create",
        );
    }
    match op {
        Op::Create { .. } => reject(
            plan,
            "plan_exists",
            format!("an active plan already exists: {}", plan.id),
            "work that plan instead: finish its steps, do not create another one. The current plan is in your context above",
        ),
        Op::Finish { id, ids, summary } => finish(plan, &id, &ids, summary),
        Op::Cancel { id, reason } => cancel(plan, id.as_deref(), reason),
        Op::Abandon { reason } => {
            let quoted = validate_surrender_reason(&reason, "abandoning a plan")?;
            abandon(plan);
            accept(plan, format!("plan {} abandoned: {quoted}", plan.id))
        }
        Op::Add {
            after,
            title,
            check,
        } => add(plan, after.as_deref(), title, check, limits),
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

pub(crate) fn reject(
    _plan: &Plan,
    code: &'static str,
    reason: impl Into<String>,
    hint: impl Into<String>,
) -> Result<Applied, Rejection> {
    Err(Rejection::new(code, reason, hint))
}

pub(crate) fn accept(plan: &mut Plan, message: impl Into<String>) -> Result<Applied, Rejection> {
    plan.revision += 1;
    Ok(Applied::Updated {
        message: message.into(),
    })
}

fn step_status(plan: &Plan, id: &str) -> Option<StepStatus> {
    plan.step(id).map(|s| s.status)
}

fn unknown_step(plan: &mut Plan, id: &str) -> Result<Applied, Rejection> {
    reject(
        plan,
        "unknown_step",
        format!("no step {id} in this plan"),
        "the current steps with their ids are in your context above",
    )
}

fn finish(plan: &mut Plan, id: &str, ids: &[String], summary: String) -> Result<Applied, Rejection> {
    if summary.trim().is_empty() {
        let label = if !id.trim().is_empty() { id.trim() } else { "current" };
        return reject(
            plan,
            "empty_summary",
            format!("step {label} needs a summary of what was done"),
            "one line: what changed and where",
        );
    }
    let targets: Vec<String> = if !ids.is_empty() {
        ids.iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else if id.contains(',') {
        id.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else if !id.trim().is_empty() {
        vec![id.trim().to_string()]
    } else {
        plan.steps
            .iter()
            .find(|s| s.status == StepStatus::InProgress)
            .or_else(|| plan.steps.iter().find(|s| s.status == StepStatus::Reopened))
            .map(|s| vec![s.id.clone()])
            .unwrap_or_default()
    };
    if targets.is_empty() {
        return unknown_step(plan, id);
    }

    for target_id in &targets {
        let Some(status) = step_status(plan, target_id) else {
            return unknown_step(plan, target_id);
        };
        if matches!(status, StepStatus::Done | StepStatus::Cancelled) {
            return reject(
                plan,
                "step_already_closed",
                format!("step {target_id} is already {}", status.as_str()),
                "only an in-progress or reopened step can be finished",
            );
        }
        if status == StepStatus::Pending {
            if let Some(active) = plan
                .steps
                .iter()
                .find(|s| s.id != *target_id && s.status == StepStatus::InProgress)
            {
                return reject(
                    plan,
                    "step_not_in_progress",
                    format!(
                        "step {target_id} is pending while step {} is in progress",
                        active.id
                    ),
                    format!("finish or cancel step {} first (or pass ids: [\"{}\", \"{target_id}\"] if both are done)", active.id, active.id),
                );
            }
        }
        let step = plan.step_mut(target_id).expect("checked above");
        step.status = StepStatus::Done;
        step.finished = Some(now());
        step.summary = Some(summary.trim().to_string());
        step.reason = None;
        auto_advance(plan);
    }

    let closed_label = targets.join(", ");
    let all_closed = plan
        .steps
        .iter()
        .all(|s| matches!(s.status, StepStatus::Done | StepStatus::Cancelled));
    if all_closed {
        plan.status = PlanStatus::Completed;
        plan.revision += 1;
        return Ok(Applied::Completed {
            message: format!(
                "step {closed_label} done — all steps closed, plan {} completed",
                plan.id
            ),
        });
    }

    let next_hint = match plan
        .steps
        .iter()
        .find(|s| matches!(s.status, StepStatus::InProgress | StepStatus::Reopened))
    {
        Some(next) => format!(
            " (next step in progress: {} \"{}\")",
            next.id,
            next.title.chars().take(48).collect::<String>()
        ),
        None => String::new(),
    };
    accept(
        plan,
        format!(
            "step {closed_label} done{next_hint} — a decision this step settled, an approach it rejected or a \
             lesson it learned belongs in `note`; the transcript gets compacted, the journal does not"
        ),
    )
}

fn cancel(plan: &mut Plan, id: Option<&str>, reason: String) -> Result<Applied, Rejection> {
    let Some(id) = id.map(str::trim).filter(|s| !s.is_empty()) else {
        return reject(
            plan,
            "need_step_id",
            "cancel needs a step id".to_string(),
            "cancel a step that will not happen, or use op=\"abandon\" with a reason to abandon the whole plan"
                .to_string(),
        );
    };
    if id == plan.id {
        return reject(
            plan,
            "abandon_user_only",
            format!("plan {} cannot be cancelled as a step", plan.id),
            "use op=\"abandon\" with a quoted reason to abandon the whole plan, or ask the user to delete it (/plan delete)"
                .to_string(),
        );
    }
    let Some(status) = step_status(plan, id) else {
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
        reason.trim().to_string()
    };
    let step = plan.step_mut(id).expect("checked above");
    step.status = StepStatus::Cancelled;
    step.reason = Some(reason_str);

    auto_advance(plan);

    let all_closed = plan
        .steps
        .iter()
        .all(|s| matches!(s.status, StepStatus::Done | StepStatus::Cancelled));
    if all_closed {
        plan.status = PlanStatus::Completed;
        plan.revision += 1;
        return Ok(Applied::Completed {
            message: format!(
                "step {id} cancelled — all steps closed, plan {} completed",
                plan.id
            ),
        });
    }

    let next_hint = match plan
        .steps
        .iter()
        .find(|s| matches!(s.status, StepStatus::InProgress | StepStatus::Reopened))
    {
        Some(next) => format!(
            " (next step in progress: {} \"{}\")",
            next.id,
            next.title.chars().take(48).collect::<String>()
        ),
        None => String::new(),
    };
    accept(plan, format!("step {id} cancelled{next_hint}"))
}

fn add(
    plan: &mut Plan,
    after: Option<&str>,
    title: String,
    check: Option<String>,
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
            "merge or cancel steps first, or ask the user to raise [plan].max_steps in the config",
        );
    }
    let index = match after.map(str::trim).filter(|s| !s.is_empty()) {
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
                        "the current steps with their ids are in your context above",
                    );
                }
            }
        }
    };
    let id = next_id(plan)?;
    let step = Step {
        id: id.clone(),
        title: title.trim().to_string(),
        check: normalize_check(check),
        status: StepStatus::Pending,
        started: None,
        finished: None,
        summary: None,
        reason: None,
        evidence: Vec::new(),
        step_epoch: 0,
    };
    plan.steps.insert(index, step);
    auto_advance(plan);
    accept(plan, format!("step {id} added"))
}
