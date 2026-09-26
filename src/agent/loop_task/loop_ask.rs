use super::{AgentEvent, ApprovalDecision, AskOption, AskQuestion, ControlMsg};
use crate::agent::tools::{self, ToolCtx};
use crate::agent::{checkpoints, safety};
use crate::plan;
use crate::providers::ToolCallReq;
use tokio::sync::mpsc;


pub(crate) async fn ask_user(
    call: &ToolCallReq,
    tx: &mpsc::Sender<AgentEvent>,
    ctl: &mut mpsc::Receiver<ControlMsg>,
    next_id: &mut u64,
) -> tools::Outcome {
    let id = *next_id;
    *next_id += 1;
    // Support both single-question (legacy) and multi-question (questions array) modes.
    let questions: Vec<AskQuestion> =
        if let Some(arr) = call.args.get("questions").and_then(|v| v.as_array()) {
            arr.iter()
                .map(|q| {
                    let header = q
                        .get("header")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let question = q
                        .get("question")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let options: Vec<AskOption> = q
                        .get("options")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .map(|o| AskOption {
                                    label: o
                                        .get("label")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    description: o
                                        .get("description")
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string()),
                                    recommended: o
                                        .get("recommended")
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(false)
                                        || o.get("label")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .contains("(Recommended)"),
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let multiple = q.get("multiple").and_then(|v| v.as_bool()).unwrap_or(false);
                    let allow_free = q
                        .get("allow_free")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true);
                    // Also check top-level custom flag as alias
                    let allow_free = if q.get("custom").and_then(|v| v.as_bool()).is_some() {
                        q.get("custom").and_then(|v| v.as_bool()).unwrap()
                    } else {
                        allow_free
                    };
                    AskQuestion {
                        header,
                        question,
                        options,
                        multiple,
                        allow_free,
                    }
                })
                .collect()
        } else {
            let question = call
                .args
                .get("question")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let options: Vec<AskOption> = call
                .args
                .get("options")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .map(|o| AskOption {
                            label: o
                                .get("label")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            description: o
                                .get("description")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string()),
                            recommended: o
                                .get("recommended")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false)
                                || o.get("label")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .contains("(Recommended)"),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let multiple = call
                .args
                .get("multiple")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let allow_free = call
                .args
                .get("allow_free")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            vec![AskQuestion {
                header: "".to_string(),
                question,
                options,
                multiple,
                allow_free,
            }]
        };

    // Small and open models often emit ask_user with no arguments at all.
    // An empty popup is useless to the user, so refuse the call and hand the
    // model the exact shape to retry with instead of blocking on the UI.
    if questions.is_empty() || questions.iter().any(|q| q.question.is_empty()) {
        return tools::Outcome::err(
            "ask_user rejected: 'question' is empty. Call it again with a non-empty question, \
             e.g. {\"question\": \"Which web framework should I use?\", \"options\": \
             [{\"label\": \"FastAPI\"}, {\"label\": \"Flask\"}], \"multiple\": false, \
             \"allow_free\": true} or with \"questions\": [{\"header\": \"Q1\", \"question\": \"...\", \"options\": [...] }].",
        );
    }

    if tx
        .send(AgentEvent::AskUser { id, questions })
        .await
        .is_err()
    {
        return tools::Outcome::err("tui closed while asking");
    }
    loop {
        match ctl.recv().await {
            Some(ControlMsg::AskAnswer { id: aid, text }) if aid == id => {
                return tools::Outcome::ok(text);
            }
            Some(_) => continue,
            None => return tools::Outcome::err("agent cancelled while asking"),
        }
    }
}

/// `propose_reset`: the agent claims the plan itself is wrong and asks the
/// user to abandon it — through the same approval dialog as dangerous
/// commands (deny preselected), never a text "yes". The quoted plan defect
/// is validated before the dialog opens, so the user never confirms a blank
/// surrender; the old plan stays on disk as Abandoned and the evidence stays
/// journaled. A replacement, if any, goes through a fresh `plan create`
/// with all its gates — reset alone cannot smuggle one in.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn propose_reset(
    call: &ToolCallReq,
    ctx: &mut tools::ToolCtx,
    read_only: bool,
    tx: &mpsc::Sender<AgentEvent>,
    ctl: &mut mpsc::Receiver<ControlMsg>,
    next_id: &mut u64,
) -> tools::Outcome {
    let root = ctx.root.clone();
    let root = root.as_path();
    // the call itself writes nothing, but an approved reset is stored by
    // the host — which a read-only session must never do (same rule as
    // propose_plan: present the problem in the answer instead)
    if read_only {
        return tools::Outcome::err(
            "project is read-only because another sqwai instance owns the lock; \
             plan reset cannot be stored — describe the plan defect in your answer instead",
        );
    }
    let session_id = ctx.session_id.clone();
    let reason = call
        .args
        .get("reason")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let mut active = match plan::open_active_for_session(root, Some(&session_id)) {
        Ok(Some(plan)) => plan,
        Ok(None) => {
            return tools::Outcome::err("no active plan: nothing to reset".to_string());
        }
        Err(error) => {
            return tools::Outcome::err(format!("active plan unreadable: {error:#}"));
        }
    };
    if let Err(rejection) = plan::validate_surrender_reason(reason, "proposing a reset") {
        return tools::Outcome::err(format!(
            "plan reset rejected [{}]: {} — {}",
            rejection.code, rejection.reason, rejection.hint
        ));
    }
    let plan_id = active.id.clone();
    let discards = plan::reset_discards(&active);
    let aid = *next_id;
    *next_id += 1;
    if tx
        .send(AgentEvent::Approval {
            id: aid,
            command: format!("abandon plan {plan_id}"),
            reason: format!(
                "quoted plan defect: {reason}\nDiscards: {discards}.\nHistory stays on disk as Abandoned; evidence stays journaled."
            ),
        })
        .await
        .is_err()
    {
        return tools::Outcome::err("tui closed awaiting reset confirm");
    }
    let decision = loop {
        match ctl.recv().await {
            Some(ControlMsg::ApprovalAnswer { id, decision }) if id == aid => break decision,
            Some(_) => continue,
            None => return tools::Outcome::err("agent cancelled awaiting reset confirm"),
        }
    };
    // blanket pre-approval is never honored for abandonment: a deliberate
    // "always" click still approves only this reset, said out loud
    let downgraded = decision == ApprovalDecision::AlwaysSession;
    if decision == ApprovalDecision::Deny {
        return tools::Outcome::err(
            "plan reset denied by user — keep working the active plan, or block it with a quoted conflict",
        );
    }
    match plan::apply(
        &mut active,
        plan::Op::ProposeReset {
            reason: reason.to_string(),
        },
        &plan::Limits::default(),
        ctx.current_step.as_deref(),
    ) {
        Ok(_) => {
            if let Err(error) = plan::store(&ctx.root, &active) {
                return tools::Outcome::err(format!("plan write failed: {error:#}"));
            }
            if let Err(error) = plan::commit(
                &ctx.root,
                &ctx.session_id,
                &mut active,
                "propose_reset",
                "user",
                true,
                serde_json::json!({"reason": reason, "plan_id": plan_id}),
            ) {
                return tools::Outcome::err(format!("plan write failed: {error:#}"));
            }
            // the session held a step of a dead plan
            ctx.current_step = None;
            let _ = tx.send(AgentEvent::StepCurrent { step: None }).await;
            tools::Outcome::ok(format!(
                "plan {plan_id} abandoned by user approval{}; start over with plan create",
                if downgraded {
                    " (always-allow treated as one-time: abandonment is never pre-approved)"
                } else {
                    ""
                }
            ))
        }
        Err(rejection) => tools::Outcome::err(format!(
            "plan reset rejected [{}]: {} — {}",
            rejection.code, rejection.reason, rejection.hint
        )),
    }
}

/// `propose_plan`: the agent proposes a full plan draft but writes nothing.
/// Two-stage host gate: the draft is validated before the user sees it (a
/// malformed draft rejects this call), then the user accepts or declines.
/// Accept abandons the active plan (if any) and stores the draft; decline
/// returns ok so the agent asks what was wrong and continues.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn propose_plan(
    call: &ToolCallReq,
    ctx: &mut ToolCtx,
    plan_limits: &crate::config::PlanConfig,
    context_limit: u64,
    read_only: bool,
    journal: &mut Option<crate::agent::journal::Journal>,
    tx: &mpsc::Sender<AgentEvent>,
    ctl: &mut mpsc::Receiver<ControlMsg>,
    next_id: &mut u64,
    session_id: &str,
) -> tools::Outcome {
    let root = ctx.root.clone();
    let root = root.as_path();
    // the call itself writes nothing, but an accepted proposal is stored by
    // the host — which a read-only session must never do (lock owned elsewhere)
    if read_only {
        return tools::Outcome::err(
            "project is read-only because another sqwai instance owns the lock; \
             plan proposals cannot be stored — present the plan in your answer instead",
        );
    }
    let id = *next_id;
    *next_id += 1;
    let mut draft_args: plan::PlanDraftArgs = match serde_json::from_value(call.args.clone()) {
        Ok(args) => args,
        Err(e) => {
            return tools::Outcome::err(format!(
                "plan proposal rejected: bad arguments ({e}) — send goal and steps"
            ));
        }
    };
    // Named verify commands expand here, before the draft is shown: what the
    // user approves has to be what gets stored, and `plan create` already
    // expands them at the same point.
    match plan::substitute_verify_commands(
        draft_args.acceptance,
        &crate::config::Config::project_verify_commands(root),
    ) {
        Ok(expanded) => draft_args.acceptance = expanded,
        Err(unknown) => {
            let hint = if unknown.known.is_empty() {
                "no verify commands seeded — run /init or write the command out".to_string()
            } else {
                format!("known: {}", unknown.known.join(", "))
            };
            return tools::Outcome::err(format!(
                "plan proposal rejected [unknown_verify]: acceptance refers to unknown \
                 verify command(s): ${} — {hint}",
                unknown.names.join(", $")
            ));
        }
    }
    let limits = plan::Limits {
        max_steps: plan_limits.max_steps,
    };
    let budget_limit = plan_limits
        .budget_tokens(context_limit)
        .max(tools::MIN_PLAN_BUDGET_TOKENS);
    let active_plan = match plan::open_active_for_session(root, Some(session_id)) {
        Ok(plan) => plan,
        Err(e) => return tools::Outcome::err(format!("active plan unreadable: {e:#}")),
    };
    if let Err(r) = plan::validate_proposal_invariants(active_plan.as_ref(), &draft_args) {
        return tools::Outcome::err(format!(
            "plan proposal violates invariants [{}]: {} — {}",
            r.code, r.reason, r.hint
        ));
    }
    let draft = match draft_args.build(budget_limit, &limits) {
        Ok(draft) => draft,
        Err(r) => {
            return tools::Outcome::err(format!(
                "plan proposal rejected [{}]: {} — {}",
                r.code, r.reason, r.hint
            ));
        }
    };
    if let Some(writer) = journal.as_mut() {
        let _ = writer.append(
            "plan",
            serde_json::json!({"op": "propose", "goal": draft.goal.text, "by": "model"}),
        );
    }
    if tx
        .send(AgentEvent::PlanProposal {
            id,
            draft: draft.clone(),
        })
        .await
        .is_err()
    {
        return tools::Outcome::err("tui closed while proposing");
    }
    let accept = loop {
        match ctl.recv().await {
            Some(ControlMsg::PlanAnswer { id: aid, accept }) if aid == id => break accept,
            Some(_) => continue,
            None => return tools::Outcome::err("agent cancelled while proposing"),
        }
    };
    if !accept {
        if let Some(writer) = journal.as_mut() {
            let _ = writer.append(
                "plan",
                serde_json::json!({"op": "decline_proposal", "by": "user"}),
            );
        }
        return tools::Outcome::ok(
            "the user declined the proposed plan. Ask what was wrong with it, adjust, \
             and propose again — do not stop working.",
        );
    }
    // Rebuild defensively: same args, same limits, fresh id. An accept can
    // only fail here on state that changed while the user was deciding.
    let mut fresh = match draft_args.build(budget_limit, &limits) {
        Ok(fresh) => fresh,
        Err(r) => {
            return tools::Outcome::err(format!(
                "accepted plan failed re-validation [{}]: {} — {}",
                r.code, r.reason, r.hint
            ));
        }
    };
    let abandoned = match plan::open_active_for_session(root, Some(session_id)) {
        Ok(Some(active)) => Some(active.id.clone()),
        Ok(None) => None,
        Err(e) => return tools::Outcome::err(format!("plan store unreadable: {e:#}")),
    };
    // §12.12: the same proof `plan create` takes, at the same moment — the
    // tree is still the pre-change one while the user is deciding.
    let proof = tools::capture_baselines(ctx, &fresh);
    plan::set_baselines(&mut fresh, proof.slots.clone());
    plan::set_snapshots(&mut fresh, proof.frozen.clone());
    plan::set_shapes(&mut fresh, proof.shapes.clone());
    plan::set_inputs(&mut fresh, proof.inputs.clone());
    // Journal-first (§2.1.4): the intent carries the full draft so replay
    // can rebuild the new plan and retire the old one after a crash.
    let new_id = fresh.id.clone();
    let new_created = fresh.created.clone();
    let intent_seq = if let Some(writer) = journal.as_mut() {
        writer
            .append(
                "plan",
                serde_json::json!({
                    "op": "accept_proposal",
                    "by": "user",
                    "ok": true,
                    "plan_id": new_id,
                    "draft": draft_args,
                    "baselines": proof.slots,
                    "snapshots": proof.frozen,
                    "shapes": proof.shapes,
                    "inputs": proof.inputs,
                    "new_id": new_id,
                    "new_created": new_created,
                    "new_sessions": [session_id],
                    "abandoned": abandoned,
                }),
            )
            .ok()
    } else {
        None
    };
    if let Some(old) = abandoned.clone() {
        match plan::open_active_for_session(root, Some(session_id)) {
            Ok(Some(mut active)) if active.id == old => {
                plan::abandon(&mut active);
                if let Some(seq) = intent_seq {
                    active.applied_event = Some(format!("{session_id}:{seq}"));
                }
                if let Err(e) = plan::store(root, &active) {
                    return tools::Outcome::err(format!(
                        "abandoning the previous plan failed: {e:#}"
                    ));
                }
            }
            Ok(_) => {}
            Err(e) => return tools::Outcome::err(format!("plan store unreadable: {e:#}")),
        }
    }
    fresh.sessions = vec![session_id.to_string()];
    if let Some(seq) = intent_seq {
        fresh.applied_event = Some(format!("{session_id}:{seq}"));
    }
    let steps = fresh.steps.len();
    if let Err(e) = plan::store(root, &fresh) {
        return tools::Outcome::err(format!("plan write failed: {e:#}"));
    }
    // tell the TUI the stored plan's id so it re-links the session before the
    // tool outcome is processed
    let _ = tx
        .send(AgentEvent::PlanAccepted { id: new_id.clone() })
        .await;
    // the TUI derives its todos panel and plan label from disk; push the new
    // plan's steps so they refresh without waiting for the next plan op
    let plan_todos: Vec<String> = fresh
        .steps
        .iter()
        .map(|s| format!("[{}] {}", s.status.as_str(), s.title))
        .collect();
    let _ = tx.send(AgentEvent::Todos(plan_todos)).await;
    tools::Outcome::ok(match abandoned {
        Some(old) => format!(
            "plan {new_id} accepted with {steps} steps; previous plan {old} abandoned. \
             Start its first step.{}",
            proof.notes.join("")
        ),
        None => format!(
            "plan {new_id} accepted with {steps} steps. Start its first step.{}",
            proof.notes.join("")
        ),
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn bash_call(
    call: &ToolCallReq,
    ctx: &mut ToolCtx,
    tx: &mpsc::Sender<AgentEvent>,
    ctl: &mut mpsc::Receiver<ControlMsg>,
    always_allow: &mut Vec<String>,
    blocked: &[String],
    next_id: &mut u64,
    subagent_depth: u8,
) -> tools::Outcome {
    let command = call.args["command"].as_str().unwrap_or("").to_string();
    let lower = command.to_lowercase();

    // -1. writer-subagent scope: the loop's bash path bypasses
    // `tools::execute`, so the scope gate lives here too. Explicit
    // out-of-scope write targets refuse before any policy prompt.
    if ctx.subagent_step.is_some()
        && let Some(allowed) = ctx.subagent_write_paths.as_ref()
        && let Some(target) = tools::bash_scope_hit(ctx, allowed, &command)
    {
        return tools::Outcome::err(
            serde_json::json!({
                "ok": false,
                "code": "subagent_scope",
                "reason": format!(
                    "'{target}' is outside this subagent's declared write scope ({})",
                    allowed.join(", ")
                ),
                "hint": "stay inside the spawned scope, or spawn with wider paths",
            })
            .to_string(),
        );
    }

    // 0. hard block from config — no questions
    for pat in blocked {
        match regex::Regex::new(pat) {
            Ok(re) => {
                if re.is_match(&command) || re.is_match(&lower) {
                    return tools::Outcome::err(format!(
                        "command blocked by [safety].blocked_patterns '{pat}'"
                    ));
                }
            }
            Err(e) => {
                return tools::Outcome::err(format!(
                    "invalid [safety].blocked_patterns regex '{pat}': {e} - command blocked"
                ));
            }
        }
    }

    // 1. heuristic dangerous-command detector
    let mut needs_approval =
        match safety::classify_for(crate::agent::shell::ShellKind::detect(), &command) {
            safety::Verdict::Safe => None,
            safety::Verdict::Blocked(reason) => {
                return tools::Outcome::err(format!("error: {reason}"));
            }
            safety::Verdict::NeedsApproval(reason) => Some(reason.to_string()),
        };

    // 1b. trust gate (R, §2.2): external taint + an egress-shaped command
    // needs the same approval with a trust reason; headless contexts deny
    // instead. Runs after the safety verdict so a Blocked command never
    // reaches here; a command both dangerous and exfiltrating carries one
    // combined reason into the single dialog.
    match crate::agent::trust::trust_gate(
        &command,
        crate::agent::trust::taint_level(&ctx.root, &ctx.session_id).external,
        subagent_depth > 0,
    ) {
        crate::agent::trust::Gate::Allow => {}
        crate::agent::trust::Gate::Deny(reason) => {
            return tools::Outcome::err(format!("command denied ({reason})"));
        }
        crate::agent::trust::Gate::Confirm(reason) => {
            needs_approval = Some(match needs_approval {
                Some(safety) => format!("{safety}; {reason}"),
                None => reason,
            });
        }
    }

    // 1c. frozen check inputs: a shell write to a test/fixture the plan
    // froze needs the same approval; headless contexts deny instead.
    // Best-effort token matching — what slips past still meets the
    // receipt-time hash comparison.
    if let Some(hit) = tools::frozen_input_command_hit(&ctx.root, &ctx.session_id, &command) {
        needs_approval = Some(match needs_approval {
            Some(prior) => format!("{prior}; {hit}"),
            None => hit,
        });
    }

    // 1d. typed constraints, live half: `forbid-cmd:` refuses outright
    // (no approval dialog — the waiver is the override). Host-run
    // acceptance commands never pass through here, only model calls.
    if let Ok(Some(plan)) = plan::open_active_for_session(&ctx.root, Some(&ctx.session_id)) {
        let waived = plan::waived_constraint_indices(&plan);
        let patterns: Vec<String> = plan
            .constraints
            .iter()
            .enumerate()
            .filter(|(index, _)| !waived.contains(index))
            .filter_map(|(_, text)| match plan::classify_constraint(text) {
                plan::ConstraintKind::ForbidCmd(pattern) => Some(pattern.to_string()),
                _ => None,
            })
            .collect();
        if let Some(hit) = tools::forbidden_command(&patterns, &command) {
            return tools::Outcome::err(format!(
                "constraint_violated: command matches forbidden pattern '{hit}' — have the user waive it (/plan waive-constraint <index> <reason>)"
            ));
        }
    }

    if let Some(reason) = &needs_approval {
        if !always_allow.contains(&command) {
            if subagent_depth > 0 {
                return tools::Outcome::err(format!(
                    "dangerous command requires user approval, but subagents cannot prompt for approval ({reason})"
                ));
            }
            let id = *next_id;
            *next_id += 1;
            if tx
                .send(AgentEvent::Approval {
                    id,
                    command: command.clone(),
                    reason: reason.to_string(),
                })
                .await
                .is_err()
            {
                return tools::Outcome::err("tui closed awaiting approval");
            }
            let decision = loop {
                match ctl.recv().await {
                    Some(ControlMsg::ApprovalAnswer { id: aid, decision }) if aid == id => {
                        break decision;
                    }
                    Some(_) => continue,
                    None => return tools::Outcome::err("agent cancelled awaiting approval"),
                }
            };
            match decision {
                ApprovalDecision::Deny => {
                    return tools::Outcome::err(format!("command denied by user ({reason})"));
                }
                ApprovalDecision::AlwaysSession => always_allow.push(command.clone()),
                ApprovalDecision::RunOnce => {}
            }
        }
        // checkpoint before running: dangerous-approved commands always;
        // otherwise only if the tree moved since this chain's last snapshot
        // (hash-gate, §2.5). The classifier cannot see mutations, only risk:
        // formatters and checkout-class commands look safe and mutate
        // silently, and must not run uninsured.
        // §2.5: bash is the one case whose targets cannot be known in
        // advance, so this is where layer 2 earns its existence.
        let snapshot_wanted = needs_approval.is_some()
            || checkpoints::tree_changed(&ctx.root, ctx.shadow_store, ctx.checkpoint_chain());
        if snapshot_wanted
            && let Ok(Some(sha)) = checkpoints::snapshot_session(
                &ctx.root,
                ctx.shadow_store,
                ctx.checkpoint_chain(),
                &format!("pre_bash {command}"),
            )
        {
            ctx.journal.push((sha, format!("bash {command}")));
        }
    }

    // 2. run it through the normal bash handler on a blocking thread so a long
    //    command never stalls the async runtime (and the TUI render loop)
    run_tool_blocking(ctx, "bash", &call.args).await
}

/// Execute a tool handler on a dedicated blocking thread.
///
/// `tools::execute` can run for a long time (e.g. `bash` up to its timeout), and
/// calling it directly inside this async task would occupy a tokio worker —
/// which, when the scheduler places `run_agent` on the `block_on` driver thread,
/// freezes the TUI for the whole call. `spawn_blocking` uses a separate thread
/// pool, so the runtime (and the UI) stay responsive. The cloned `ToolCtx` is
/// moved in and its journal/checkpoint bookkeeping is merged back afterwards so
/// callers observe every mutation the handler recorded.
pub(crate) async fn run_tool_blocking(
    ctx: &mut ToolCtx,
    name: &str,
    args: &serde_json::Value,
) -> tools::Outcome {
    let mut exec_ctx = ctx.clone();
    let fallback_ctx = exec_ctx.clone();
    let name = name.to_string();
    let args = args.clone();
    let (outcome, exec_ctx) = tokio::task::spawn_blocking(move || {
        let o = tools::execute(&mut exec_ctx, &name, &args);
        (o, exec_ctx)
    })
    .await
    .unwrap_or_else(|e| {
        (
            tools::Outcome::err(format!("tool thread failed: {e}")),
            fallback_ctx,
        )
    });
    ctx.journal = exec_ctx.journal;
    ctx.files_read = exec_ctx.files_read;
    outcome
}

pub(crate) fn is_accepted_memory_answer(answer_ok: bool, raw_answer: &str) -> bool {
    let raw = raw_answer.trim();
    let lower = raw.to_ascii_lowercase();
    answer_ok
        && (lower == "accept"
            || (!raw.is_empty()
                && lower != "reject"
                && lower != "edit"
                && !lower.starts_with("subagents cannot")))
}

