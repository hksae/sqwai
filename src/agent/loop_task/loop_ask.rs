use super::{
    ASK_MAX_OPTIONS, ASK_MAX_QUESTIONS, ASK_MIN_OPTIONS, AgentEvent, ApprovalDecision, AskOption,
    AskQuestion, ControlMsg,
};
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
    // Support both single-question (legacy) and multi-question (questions array)
    // modes. The bounds are not trimmed away here: a call that breaks them is
    // refused by `ask_rejection` with the shape to resend, because silently
    // dropping the model's fifth question leaves it believing it asked.
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
                    let options = parse_options(q.get("options"));
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
            let options = parse_options(call.args.get("options"));
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

    // Small and open models often emit ask_user with no arguments at all, or
    // with the shape they had in mind rather than the one the schema asks for.
    // A blank questionnaire is useless to the user and reads back to the model
    // as "(no answer)" — which it then retries, three calls deep. Refuse the
    // call and name the shape to send instead.
    if let Some(why) = ask_rejection(&questions) {
        return tools::Outcome::err(why);
    }

    if tx
        .send(AgentEvent::AskUser {
            id,
            call_id: call.id.clone(),
            questions,
        })
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

/// Options in either shape models actually send: the schema's
/// `[{"label": …, "description": …}]`, or bare `["FastAPI", "Flask"]` strings,
/// which small and free models emit constantly. A missing label falls back to
/// the description; an option with no text at all stays empty, and
/// [`ask_rejection`] refuses the call rather than painting a row nobody can
/// pick.
fn parse_options(src: Option<&serde_json::Value>) -> Vec<AskOption> {
    src.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .map(|o| {
                    let text = |key: &str| o.get(key).and_then(|v| v.as_str()).map(str::to_string);
                    let label = o
                        .as_str()
                        .map(str::to_string)
                        .or_else(|| text("label"))
                        .or_else(|| text("title"))
                        .or_else(|| text("text"))
                        .or_else(|| text("description"))
                        .unwrap_or_default();
                    let description = text("description").filter(|d| d.trim() != label.trim());
                    let recommended = o
                        .get("recommended")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                        || label.contains("(Recommended)");
                    AskOption {
                        label,
                        description,
                        recommended,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Why this call must not reach the UI, in the words the model needs to fix
/// it. `None` means the questionnaire is answerable as built.
fn ask_rejection(questions: &[AskQuestion]) -> Option<String> {
    if questions.is_empty() {
        return Some(
            "ask_user rejected: no question given. Send {\"question\": \"…\", \"options\": \
             [\"a\", \"b\"]} or \"questions\": [{\"header\": \"Q1\", \"question\": \"…\", \
             \"options\": [\"a\", \"b\"]}]."
                .to_string(),
        );
    }
    if questions.len() > ASK_MAX_QUESTIONS {
        return Some(format!(
            "ask_user rejected: {} questions, and one call carries at most {ASK_MAX_QUESTIONS}. \
             Ask the first {ASK_MAX_QUESTIONS} now; the rest belong to the next call, after the \
             user has answered these.",
            questions.len()
        ));
    }
    if questions.iter().any(|q| q.question.trim().is_empty()) {
        return Some(
            "ask_user rejected: 'question' is empty. Call it again with a non-empty question, \
             e.g. {\"question\": \"Which web framework should I use?\", \"options\": \
             [\"FastAPI\", \"Flask\"]}."
                .to_string(),
        );
    }
    let wrong_count = questions
        .iter()
        .position(|q| q.options.len() < ASK_MIN_OPTIONS || q.options.len() > ASK_MAX_OPTIONS);
    if let Some(i) = wrong_count {
        return Some(format!(
            "ask_user rejected: question {} has {} options; every question needs \
             {ASK_MIN_OPTIONS}-{ASK_MAX_OPTIONS}. Offer a real choice, or ask in plain text and \
             let the user answer freely.",
            i + 1,
            questions[i].options.len()
        ));
    }
    let blank = questions
        .iter()
        .position(|q| q.options.iter().any(|o| o.label.trim().is_empty()));
    blank.map(|i| {
        format!(
            "ask_user rejected: question {} has an option with no text, which the user cannot \
             read or pick. Send options as strings ([\"FastAPI\", \"Flask\"]) or as objects \
             with a non-empty \"label\" ([{{\"label\": \"FastAPI\", \"description\": \"…\"}}]).",
            i + 1
        )
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn bash_call(
    call: &ToolCallReq,
    ctx: &mut ToolCtx,
    tx: &mpsc::Sender<AgentEvent>,
    ctl: &mut mpsc::Receiver<ControlMsg>,
    always_allow: &mut Vec<String>,
    blocked: &[(String, regex::Regex)],
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

    // 0. hard block from config — no questions. The regexes arrive compiled by
    // the run that owns this call; an invalid pattern is refused at startup,
    // so there is nothing here to report but a match.
    for (pat, re) in blocked {
        if re.is_match(&command) || re.is_match(&lower) {
            return tools::Outcome::err(format!(
                "command blocked by [safety].blocked_patterns '{pat}'"
            ));
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
    // The three accumulating maps move into the worker thread instead of being
    // copied through a `ToolCtx` clone (#193) — `files_read` and `read_windows`
    // grow over a whole session, and this call runs for every tool call. What
    // stays behind is one insurance copy, used only if the closure dies mid-run
    // and takes the live maps with it.
    let files_read = std::mem::take(&mut ctx.files_read);
    let read_windows = std::mem::take(&mut ctx.read_windows);
    let journal = std::mem::take(&mut ctx.journal);
    let fallback = (files_read.clone(), read_windows.clone(), journal.clone());
    let mut exec_ctx = ctx.clone();
    exec_ctx.files_read = files_read;
    exec_ctx.read_windows = read_windows;
    exec_ctx.journal = journal;
    let name = name.to_string();
    let args = args.clone();
    let joined = tokio::task::spawn_blocking(move || {
        let outcome = tools::execute(&mut exec_ctx, &name, &args);
        (outcome, exec_ctx)
    })
    .await;
    match joined {
        Ok((outcome, worker)) => {
            ctx.journal = worker.journal;
            ctx.files_read = worker.files_read;
            ctx.read_windows = worker.read_windows;
            outcome
        }
        Err(e) => {
            let (files_read, read_windows, journal) = fallback;
            ctx.journal = journal;
            ctx.files_read = files_read;
            ctx.read_windows = read_windows;
            tools::Outcome::err(format!("tool thread failed: {e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn question(options: serde_json::Value) -> AskQuestion {
        AskQuestion {
            header: String::new(),
            question: "Which one?".into(),
            options: parse_options(Some(&options)),
            multiple: false,
            allow_free: true,
        }
    }

    #[test]
    fn options_may_be_plain_strings() {
        let opts = parse_options(Some(&json!(["FastAPI", "Flask"])));
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].label, "FastAPI");
        assert_eq!(opts[1].label, "Flask");
        assert!(opts.iter().all(|o| o.description.is_none()));
        assert!(ask_rejection(&[question(json!(["a", "b"]))]).is_none());
    }

    #[test]
    fn a_description_alone_can_carry_the_label() {
        let opts = parse_options(Some(&json!([{"description": "just an explanation"}])));
        assert_eq!(opts[0].label, "just an explanation");
        assert!(opts[0].description.is_none(), "not repeated under itself");
    }

    #[test]
    fn the_documented_shape_still_wins() {
        let opts = parse_options(Some(&json!([
            {"label": "FastAPI", "description": "typed, async", "recommended": true}
        ])));
        assert_eq!(opts[0].label, "FastAPI");
        assert_eq!(opts[0].description.as_deref(), Some("typed, async"));
        assert!(opts[0].recommended);
    }

    #[test]
    fn an_option_with_no_text_refuses_the_call_instead_of_rendering_blank() {
        let why = ask_rejection(&[question(json!(["", "  "]))]).expect("refused");
        assert!(why.contains("question 1"), "{why}");
        assert!(why.contains("no text"), "{why}");
    }

    #[test]
    fn a_question_needs_a_real_set_of_options() {
        // too few — including none, which used to pass as a free-text ask
        assert!(ask_rejection(&[question(json!([]))]).is_some());
        assert!(ask_rejection(&[question(json!(["only one"]))]).is_some());
        // too many
        let many: Vec<String> = (0..=ASK_MAX_OPTIONS).map(|i| format!("o{i}")).collect();
        assert!(ask_rejection(&[question(json!(many))]).is_some());
        // both edges of the promised range are fine
        assert!(ask_rejection(&[question(json!(["a", "b"]))]).is_none());
        let edge: Vec<String> = (0..ASK_MAX_OPTIONS).map(|i| format!("o{i}")).collect();
        assert!(ask_rejection(&[question(json!(edge))]).is_none());
    }

    #[test]
    fn a_call_with_too_many_questions_names_the_cap() {
        let qs: Vec<AskQuestion> = (0..=ASK_MAX_QUESTIONS)
            .map(|_| question(json!(["a", "b"])))
            .collect();
        let why = ask_rejection(&qs).expect("refused");
        assert!(why.contains(&ASK_MAX_QUESTIONS.to_string()), "{why}");
        assert!(why.contains("next call"), "{why}");
    }
}
