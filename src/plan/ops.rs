use super::{
    Acceptance, AcceptanceKind, AcceptanceStatus, Budget, Goal, Plan, PlanStatus, Step,
    StepRef, StepStatus, Validation, ValidationStatus, complete, new_id, next_id, now,
    render, verify_acceptance, MAX_STEPS_DEFAULT,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};


// ---------------------------------------------------------------- operations

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewStep {
    pub title: String,
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
        /// Non-blocking free-text checklist (plan-lite). Never gates
        /// complete; walked past, never settled.
        #[serde(default)]
        checklist: Vec<String>,
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
    /// Honest surrender: the task is impossible as specified. `reason`
    /// must quote the conflict (spec vs test, contradictory requirements);
    /// empty reasons are refused. Sets status `Blocked` — terminal and
    /// read-only like every closed plan, but reported as an honest result,
    /// never as failed work.
    BlockPlan {
        #[serde(default)]
        reason: String,
    },
    /// Reset proposal: the plan itself is wrong (not the work). `reason`
    /// must quote the plan defect; empty reasons are refused. Never applied
    /// directly — the dispatcher routes it to the agent loop, which asks
    /// the user through the approval dialog and only then abandons. The
    /// old plan stays on disk as `Abandoned` (history is never rewritten);
    /// a replacement, if any, goes through a fresh `create` with all its
    /// gates. Replayable like every op (see apply_record).
    ProposeReset {
        #[serde(default)]
        reason: String,
    },
    Add {
        #[serde(default)]
        after: Option<String>,
        title: String,
        #[serde(default)]
        refs: Vec<StepRef>,
    },
    /// Append acceptance criteria to a plan born without (or with fewer
    /// than needed): plan-lite grows teeth when the work turns out real.
    /// Same typing gate as create; baselines are captured by the dispatcher
    /// for the new items (best effort — a check that already passes stays
    /// unproven, correctly: the host never saw it fail).
    AddAcceptance {
        #[serde(default)]
        items: Vec<String>,
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
    #[serde(default)]
    pub checklist: Vec<String>,
}

impl PlanDraftArgs {
    pub fn build(&self, budget_limit: u64, limits: &Limits) -> Result<Plan, Rejection> {
        let mut plan = create(
            self.goal.clone(),
            self.constraints.clone(),
            self.acceptance.clone(),
            self.steps.clone(),
            budget_limit,
            limits,
        )?;
        plan.checklist = self.checklist.clone();
        Ok(plan)
    }
}

/// Host-only: retire the active plan when the user accepts a replacement
/// proposal. The reason travels in the journal; the file keeps no history
/// of why it was abandoned.
pub fn abandon(plan: &mut Plan) {
    plan.status = PlanStatus::Abandoned;
    plan.revision += 1;
}

/// What a reset would throw away, for the confirm dialog. Evidence stays
/// journaled either way — this lists what leaves the active surface.
pub fn reset_discards(plan: &Plan) -> String {
    let done = plan
        .steps
        .iter()
        .filter(|step| step.status == StepStatus::Done)
        .count();
    let open: Vec<&str> = plan
        .steps
        .iter()
        .filter(|step| {
            matches!(
                step.status,
                StepStatus::Pending | StepStatus::InProgress | StepStatus::Blocked
            )
        })
        .map(|step| step.id.as_str())
        .collect();
    let unsettled = plan
        .acceptance
        .iter()
        .filter(|item| {
            !matches!(
                item.validation.status,
                ValidationStatus::Passed | ValidationStatus::Waived
            )
        })
        .count();
    format!(
        "{} steps done, {} open ({}), {} acceptance unsettled",
        done,
        open.len(),
        open.join(", "),
        unsettled
    )
}

/// Reason gate shared by BlockPlan and ProposeReset: the surrender must
/// quote what is wrong (conflict or plan defect), not gesture at effort.
/// Empty reasons are refused; one-word reasons are refused with guidance.
pub fn validate_surrender_reason(reason: &str, what: &str) -> Result<String, Rejection> {
    let quoted = reason.trim().to_string();
    if quoted.is_empty() {
        return Err(Rejection::new(
            "empty_reason",
            format!("{what} needs the quoted conflict"),
            "cite what contradicts what: the spec line against the test or requirement".to_string(),
        ));
    }
    if quoted.split_whitespace().count() < 3 {
        return Err(Rejection::new(
            "thin_reason",
            format!("{what} reason is too thin to judge: {quoted}"),
            "quote the defect itself — which requirement, step, or criterion is wrong and why".to_string(),
        ));
    }
    Ok(quoted)
}

/// Honest surrender (`BlockPlan`): the task cannot be done as specified.
/// The quoted conflict is stored on the plan file itself — it is the
/// artifact future readers and the bench harness judge, not the journal.
fn block_plan(plan: &mut Plan, reason: String) -> Result<Applied, Rejection> {
    let quoted = match validate_surrender_reason(&reason, "blocking a plan") {
        Ok(quoted) => quoted,
        Err(rejection) => {
            plan.rejections_in_a_row += 1;
            return Err(rejection);
        }
    };
    plan.status = PlanStatus::Blocked;
    plan.blocked_reason = Some(quoted);
    plan.revision += 1;
    accept(
        plan,
        format!("plan {} blocked: spec conflict recorded", plan.id),
    )
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

/// Shared typing gate for create and add-acceptance: every criterion is
/// executable or explicitly human; free text settles on whatever evidence
/// happens to exist, which is a claim, not a check.
fn check_typed_acceptance(acceptance: &[String]) -> Result<(), Rejection> {
    for text in acceptance {
        if matches!(AcceptanceKind::classify(text), AcceptanceKind::Text(_)) {
            return Err(Rejection::new(
                "untyped_acceptance",
                format!("acceptance criterion is free text: {text}"),
                "rewrite it as cmd: (a check that fails before and passes after) \
                 or manual: (a human checks it by hand); plain notes go to \
                 checklist, which never gates anything",
            ));
        }
    }
    Ok(())
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
    // Untyped acceptance is refused, not settled: free text settles on
    // whatever evidence happens to exist, which is a claim, not a check.
    // Every criterion is executable (`cmd:`, `snapshot:`, `differential:`,
    // `signatures:`) or explicitly human (`manual:`).
    check_typed_acceptance(&acceptance)?;

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
                snapshot: None,
                shape: None,
                inputs: Vec::new(),
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
        blocked_reason: None,
        // the non-blocking checklist rides the create intent (dispatcher
        // sets it post-create, like sessions); create() itself stays
        // checklist-free so its 26 callers do not churn
        checklist: Vec::new(),
        waived_constraints: Vec::new(),
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
    // Closed plans are read-only: Show inspects, everything else belongs
    // to an active plan. Without this the model path could keep mutating
    // completed or abandoned plans (defect A).
    if plan.status != PlanStatus::Active && !matches!(op, Op::Show) {
        return reject(
            plan,
            "plan_closed",
            format!(
                "plan {} is {}",
                plan.id,
                match plan.status {
                    PlanStatus::Completed => "completed",
                    PlanStatus::Abandoned => "abandoned",
                    PlanStatus::Blocked => "blocked",
                    PlanStatus::Active => "active",
                }
            ),
            "closed plans are read-only; start a new plan with /plan",
        );
    }
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
        Op::BlockPlan { reason } => block_plan(plan, reason),
        Op::ProposeReset { reason } => {
            let quoted = match validate_surrender_reason(&reason, "proposing a reset") {
                Ok(quoted) => quoted,
                Err(rejection) => {
                    plan.rejections_in_a_row += 1;
                    return Err(rejection);
                }
            };
            abandon(plan);
            accept(
                plan,
                format!("plan {} abandoned on approved reset: {quoted}", plan.id),
            )
        }
        Op::Add {
            after,
            title,
            refs,
        } => add(plan, after.as_deref(), title, refs, limits),
        Op::AddAcceptance { items } => add_acceptance(plan, items),
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

pub(crate) fn reject(
    plan: &mut Plan,
    code: &'static str,
    reason: impl Into<String>,
    hint: impl Into<String>,
) -> Result<Applied, Rejection> {
    plan.rejections_in_a_row += 1;
    Err(Rejection::new(code, reason, hint))
}

pub(crate) fn accept(plan: &mut Plan, message: impl Into<String>) -> Result<Applied, Rejection> {
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
    // Evidence presence is validated by the host tool dispatcher (strict
    // mode only), after host journal records have been attached to this step.
    // Soft steps close on the summary above; progress reads from receipts.
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
    // No silent whole-plan kill: a missing id used to abandon the entire
    // plan, so a model that forgot the id destroyed it by accident — and a
    // model that remembered it could abandon + recreate around every
    // goal/constraint guard in two calls.
    let Some(id) = id else {
        return reject(
            plan,
            "need_step_id",
            "cancel needs a step id".to_string(),
            "cancel a step that will not happen, or ask the user to abandon the whole plan (/plan abandon)".to_string(),
        );
    };
    // Abandoning the whole plan is the user's call (TUI `/plan abandon`,
    // journaled with by: user). The model surrenders contradictions with
    // `block_plan` (quoted) instead of walking around the goal.
    if id == plan.id {
        return reject(
            plan,
            "abandon_user_only",
            format!("plan {} can only be abandoned by the user", plan.id),
            "surrender a contradiction with block_plan (quote it), or ask the user to abandon it".to_string(),
        );
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

/// Append acceptance criteria. Empty additions and free text are refused;
/// items land pending with no baseline — the dispatcher captures baselines
/// for exactly these positions (see plan_op), so replay needs no proof data.
pub(crate) fn add_acceptance(plan: &mut Plan, items: Vec<String>) -> Result<Applied, Rejection> {
    if items.iter().all(|text| text.trim().is_empty()) {
        return reject(
            plan,
            "empty_acceptance",
            "no acceptance criteria given",
            "name a check (cmd:) or a human checkpoint (manual:)",
        );
    }
    check_typed_acceptance(&items)?;
    let mut added = 0;
    for text in items {
        if text.trim().is_empty() {
            continue;
        }
        plan.acceptance.push(Acceptance {
            text,
            status: AcceptanceStatus::Pending,
            evidence: Vec::new(),
            validation: Validation::default(),
            baseline: None,
            snapshot: None,
            shape: None,
            inputs: Vec::new(),
            by: None,
            reason: None,
        });
        added += 1;
    }
    accept(plan, format!("acceptance +{added}"))
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

