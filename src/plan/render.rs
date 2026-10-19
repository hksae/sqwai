//! Plan rendering for the prompt and `/plan`. Criteria are the agent's own
//! notes; the host records what it observed and never gates on a validation
//! state.

use super::{Plan, PlanStatus, StepStatus};

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
    out.push_str(&format!(
        "steps: {} done · {} in progress · {} pending · {} cancelled\n",
        c.done,
        c.in_progress,
        c.pending + c.reopened,
        c.cancelled
    ));
    for s in &plan.steps {
        let marker = match s.status {
            StepStatus::Done => "[x]",
            StepStatus::InProgress => "[>]",
            StepStatus::Cancelled => "[-]",
            StepStatus::Pending | StepStatus::Reopened => "[ ]",
        };
        out.push_str(&format!("  {} {} {}\n", marker, s.id, s.title));
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
    }
}
