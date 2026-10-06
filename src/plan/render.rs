//! Plan rendering for the prompt and `/plan`, plus the host-only goal
//! revision. The settle/confirm/waive machinery that lived here was retired
//! 2026-10-06: criteria are the agent's own notes, the host records what it
//! observed and never gates on a validation state.

use super::{GoalRevision, Plan, PlanStatus, StepStatus, now};

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

/// The stable half of the plan: goal, constraints, criteria. Rides the
/// cacheable prefix; only changes when the user or a create does.
pub fn render_goal(plan: &Plan) -> String {
    let mut out = String::new();
    out.push_str(&format!("plan {}\n", plan.id));
    out.push_str(&format!("goal: {}\n", plan.goal.text));
    if !plan.constraints.is_empty() {
        out.push_str(&format!("constraints: {}\n", plan.constraints.join(" · ")));
    }
    if !plan.criteria.is_empty() {
        out.push_str("criteria (your own done-notes):\n");
        for note in &plan.criteria {
            out.push_str(&format!("  - {note}\n"));
        }
    }
    out
}

/// The moving half of the plan: status and steps with their summaries.
/// Rebuilt every turn, so it rides the volatile tail — step transitions must
/// never invalidate the cached prefix.
pub fn render_status(plan: &Plan) -> String {
    let c = plan.counts();
    let mut out = String::new();
    out.push_str(&format!("status: {}\n", status_word(plan.status)));
    if let Some(reason) = &plan.blocked_reason {
        out.push_str(&format!("blocked: {reason}\n"));
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
        let mut line = format!("  {} {} {}", marker, s.id, s.title);
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

pub fn render(plan: &Plan) -> String {
    format!("{}{}", render_goal(plan), render_status(plan))
}

fn status_word(status: PlanStatus) -> &'static str {
    match status {
        PlanStatus::Active => "active",
        PlanStatus::Completed => "completed",
        PlanStatus::Abandoned => "abandoned",
        PlanStatus::Blocked => "blocked",
    }
}
