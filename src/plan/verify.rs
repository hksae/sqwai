//! Plan completion and step-id generation.
//!
//! The verification machinery that lived here (state digests, receipts,
//! baselines, snapshots, shapes, the judge ladder, the `verify` settle
//! path) was retired 2026-10-06: the host records what it observed and the
//! plan's criteria are the agent's own notes, not gates. What remains is
//! `complete` — which checks only that no step is left open — and `next_id`.

use super::ops::reject;
use super::{Applied, Plan, PlanStatus, Rejection, StepStatus};

pub(crate) fn complete(plan: &mut Plan) -> Result<Applied, Rejection> {
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
    plan.status = PlanStatus::Completed;
    plan.revision += 1;
    plan.rejections_in_a_row = 0;
    Ok(Applied::Completed)
}

pub(crate) fn next_id(plan: &Plan) -> String {
    let max = plan
        .steps
        .iter()
        .filter_map(|s| s.id.parse::<usize>().ok())
        .max()
        .unwrap_or(0);
    (max + 1).to_string()
}
