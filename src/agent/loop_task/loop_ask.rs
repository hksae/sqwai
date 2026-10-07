use super::{AgentEvent, ApprovalDecision, AskOption, AskQuestion, ControlMsg};
use crate::agent::tools::{self, ToolCtx};
use crate::agent::{checkpoints, safety};
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
    // One dialog, bounded: a dump of dozens of questions is a wall the user
    // cannot answer (audit M23) — the rest waits for the next call.
    const MAX_QUESTIONS_PER_CALL: usize = 8;
    let questions: Vec<AskQuestion> =
        if let Some(arr) = call.args.get("questions").and_then(|v| v.as_array()) {
            arr.iter()
                .take(MAX_QUESTIONS_PER_CALL)
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
    journal: &mut Option<crate::agent::journal::Journal>,
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
    // combined reason into the single dialog. One dialog per (session, egress
    // kind): a kind the user already confirmed passes (§11).
    let acked = crate::agent::trust::acked_egress(&ctx.root, &ctx.session_id);
    let mut taint_kind: Option<&'static str> = None;
    match crate::agent::trust::trust_gate(
        &command,
        ctx.external_taint(),
        subagent_depth > 0,
        &acked,
    ) {
        crate::agent::trust::Gate::Allow => {}
        crate::agent::trust::Gate::Deny(reason) => {
            return tools::Outcome::err(format!("command denied ({reason})"));
        }
        crate::agent::trust::Gate::Confirm(reason) => {
            taint_kind = crate::agent::safety::egress_kind(&command);
            needs_approval = Some(match needs_approval {
                Some(safety) => format!("{safety}; {reason}"),
                None => reason,
            });
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
                // The user looked at this egress shape and said yes: journal
                // it so the same kind passes for the rest of the session.
                ApprovalDecision::AlwaysSession => {
                    if let (Some(kind), Some(writer)) = (taint_kind, journal.as_mut()) {
                        crate::agent::trust::ack_egress(writer, kind);
                    }
                    always_allow.push(command.clone());
                }
                ApprovalDecision::RunOnce => {
                    if let (Some(kind), Some(writer)) = (taint_kind, journal.as_mut()) {
                        crate::agent::trust::ack_egress(writer, kind);
                    }
                }
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
    ctx.read_windows = exec_ctx.read_windows;
    outcome
}
