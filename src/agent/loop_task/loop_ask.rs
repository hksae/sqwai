use super::{
    ASK_MAX_OPTIONS, ASK_MAX_QUESTIONS, ASK_MIN_OPTIONS, AgentEvent, ApprovalDecision, AskOption,
    AskQuestion, ControlMsg,
};
use crate::agent::tools::{self, ToolCtx};
use crate::agent::{checkpoints, safety};
use crate::config::ApprovalMode;
use crate::providers::ToolCallReq;
use tokio::sync::mpsc;

/// Reason shown for commands and calls that only ask because the session
/// runs in Ask mode. Grants earned under it cover the same reason, so one
/// "always allow this session" answer still silences the repeats.
const ASK_MODE_REASON: &str = "ask mode";
/// Network reads leave the machine, so ASK confirms each one and never
/// offers "always": the user asked to see every call.
const NETWORK_ASK_REASON: &str = "network read in ask mode";

/// Why a leap beyond the project root asks. The workspace jail is a
/// prompt, not a wall: full access never asks (every path resolves), and
/// an approval opens the target directory for reads AND writes. Host-owned
/// state (`.sqwai/`) is root-relative — it can never appear on this branch.
pub(crate) const OUTSIDE_WRITE_REASON: &str = "writes outside the project directory";
pub(crate) const OUTSIDE_READ_REASON: &str = "reads outside the project directory";

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

/// What one "always allow" answer grants for the rest of the session.
///
/// Splits compound commands (`&&`, `||`, `;`, `|`, `$(...)`) into individual
/// subcommands, strips transparent wrappers (`timeout`, `nice`, `env`), uses
/// arity-aware command heads (`git commit`, `docker compose up`), pins target
/// paths for destructive file commands (`rm -rf target`), and requires exact
/// matches for exec wrappers (`sudo`, `xargs`, `find -delete`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Grant {
    reason: String,
    subcommands: Vec<safety::SubcommandGrant>,
    opaque_exact: Option<String>,
}

impl Grant {
    pub(crate) fn from_command(command: &str, reason: &str) -> Self {
        match safety::split_compound_commands(command) {
            Ok(subs) if !subs.is_empty() => {
                let shell = crate::agent::shell::ShellKind::detect();
                let mut risky: Vec<safety::SubcommandGrant> = subs
                    .iter()
                    .filter(|s| {
                        !matches!(safety::classify_for(shell, s), safety::Verdict::Safe)
                            || safety::egress_kind(s).is_some()
                    })
                    .map(|s| safety::SubcommandGrant::from_subcommand(s))
                    .collect();
                if risky.is_empty() {
                    risky = subs
                        .iter()
                        .map(|s| safety::SubcommandGrant::from_subcommand(s))
                        .collect();
                }
                Self {
                    reason: reason.to_string(),
                    subcommands: risky,
                    opaque_exact: None,
                }
            }
            _ => Self {
                reason: reason.to_string(),
                subcommands: Vec::new(),
                opaque_exact: Some(command.trim().to_string()),
            },
        }
    }

    /// True when running `command` for `reason` is inside what was approved.
    pub(crate) fn covers(&self, command: &str, reason: &str) -> bool {
        if self.reason != reason {
            return false;
        }
        if let Some(exact) = &self.opaque_exact {
            return exact == command.trim();
        }
        let Ok(subs) = safety::split_compound_commands(command) else {
            return false;
        };
        if subs.is_empty() {
            return false;
        }
        let shell = crate::agent::shell::ShellKind::detect();
        let risky_subs: Vec<&String> = subs
            .iter()
            .filter(|s| {
                !matches!(safety::classify_for(shell, s), safety::Verdict::Safe)
                    || safety::egress_kind(s).is_some()
            })
            .collect();
        let to_check: Vec<&String> = if risky_subs.is_empty() {
            subs.iter().collect()
        } else {
            risky_subs
        };
        to_check
            .into_iter()
            .all(|sub| self.subcommands.iter().any(|g| g.covers(sub)))
    }
}

fn grants_cover_command(grants: &[Grant], command: &str, reason: &str) -> bool {
    if grants.iter().any(|g| g.covers(command, reason)) {
        return true;
    }
    let Ok(subs) = safety::split_compound_commands(command) else {
        return false;
    };
    if subs.len() <= 1 {
        return false;
    }
    let shell = crate::agent::shell::ShellKind::detect();
    let risky_subs: Vec<&String> = subs
        .iter()
        .filter(|s| {
            !matches!(safety::classify_for(shell, s), safety::Verdict::Safe)
                || safety::egress_kind(s).is_some()
        })
        .collect();
    if risky_subs.is_empty() {
        return false;
    }
    risky_subs.into_iter().all(|sub| {
        let sub_reason = match safety::classify_for(shell, sub) {
            safety::Verdict::NeedsApproval(r) => r,
            _ => reason,
        };
        grants.iter().any(|g| {
            (g.reason == reason || g.reason == sub_reason)
                && g.subcommands.iter().any(|sg| sg.covers(sub))
        })
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn bash_call(
    call: &ToolCallReq,
    ctx: &mut ToolCtx,
    tx: &mpsc::Sender<AgentEvent>,
    ctl: &mut mpsc::Receiver<ControlMsg>,
    always_allow: &mut Vec<Grant>,
    blocked: &[(String, regex::Regex)],
    next_id: &mut u64,
    subagent_depth: u8,
    approval: ApprovalMode,
    events: &mut Option<crate::agent::events::EventLog>,
    outside_target: &mut Option<std::path::PathBuf>,
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

    // What the classifier said about this command, recorded whether or not a
    // dialog followed. AUTO's whole contract is "the classifier is the floor",
    // and a floor nobody can see is a guess: a command that passed as `safe`
    // left no trace of the judgment behind it, so "why did AUTO run this" cannot
    // be answered from the journal — the same hole that made every context bug
    // this week take three passes to find. `approval` records the question and
    // the answer; this records the host's own opinion, before escalation.
    let record_verdict = |events: &mut Option<crate::agent::events::EventLog>,
                          verdict: &str,
                          reason: &str| {
        let Some(writer) = events.as_mut() else {
            return;
        };
        let _ = writer.append_evidence(
            "verdict",
            serde_json::json!({
                "tool": "bash",
                "call_id": call.id,
                "mode": approval.as_str(),
                "verdict": verdict,
                "reason": reason,
                "command": command.chars().take(200).collect::<String>(),
            }),
        );
    };

    // 0. hard block from config — no questions. The regexes arrive compiled by
    // the run that owns this call; an invalid pattern is refused at startup,
    // so there is nothing here to report but a match.
    for (pat, re) in blocked {
        if re.is_match(&command) || re.is_match(&lower) {
            record_verdict(events, "blocked_by_pattern", pat);
            return tools::Outcome::err(format!(
                "command blocked by [safety].blocked_patterns '{pat}'"
            ));
        }
    }

    // 1. heuristic dangerous-command detector
    let mut needs_approval =
        match safety::classify_for(crate::agent::shell::ShellKind::detect(), &command) {
            safety::Verdict::Safe => {
                record_verdict(events, "safe", "");
                None
            }
            safety::Verdict::Blocked(reason) => {
                record_verdict(events, "blocked", reason);
                return tools::Outcome::err(format!("error: {reason}"));
            }
            safety::Verdict::NeedsApproval(reason) => {
                record_verdict(events, "needs_approval", reason);
                Some(reason.to_string())
            }
        };

    // Subagents cannot prompt, so Ask mode would deny them every command
    // outright. The classifier's judgment (Auto) is the floor for
    // non-interactive runs; Full still lifts the prompt entirely.
    let mode = if subagent_depth > 0 && approval == ApprovalMode::Ask {
        ApprovalMode::Auto
    } else {
        approval
    };

    // 1b. trust gate: external taint + an egress-shaped command
    // needs the same approval with a trust reason; headless contexts deny
    // instead. Runs after the safety verdict so a Blocked command never
    // reaches here; a command both dangerous and exfiltrating carries one
    // combined reason into the single dialog. One dialog per (session, egress
    // kind): a kind the user already confirmed passes.
    let acked = crate::agent::trust::acked_egress(&ctx.root, &ctx.events_session);
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

    // 1c. workspace boundary as a prompt: bash writing outside the project
    // root asks, exactly like the file tools' jail. Subagent scopes
    // already refuse out-of-scope writes on their own; full access runs
    // it like every other prompt.
    if mode != ApprovalMode::Full
        && subagent_depth == 0
        && needs_approval.is_none()
        && let Some(target) = tools::first_outside_target(ctx, &command)
    {
        // remembered for the AlwaysSession answer below: the directory the
        // offending target lives in
        *outside_target = ctx.outside_grant_dir(&target);
        needs_approval = Some(OUTSIDE_WRITE_REASON.to_string());
    }

    // Ask mode: every command is confirmed, even one the classifier calls
    // safe. Grants still cover the repeats — that is what keeps Ask livable.
    if mode == ApprovalMode::Ask && needs_approval.is_none() {
        needs_approval = Some(ASK_MODE_REASON.to_string());
    }

    // Full mode runs everything the hard gates let through without a
    // dialog; `needs_approval` stays set so the pre-run checkpoint still
    // insures the dangerous ones.
    if let Some(reason) = &needs_approval
        && mode != ApprovalMode::Full
        && !grants_cover_command(always_allow, &command, reason)
    {
        if subagent_depth > 0 {
            return tools::Outcome::err(format!(
                "dangerous command requires user approval, but subagents cannot prompt for approval ({reason})"
            ));
        }
        let Some(decision) = approval_dialog(
            command.clone(),
            reason,
            safety::session_grant_preview(&command),
            true,
            tx,
            ctl,
            next_id,
            events,
        )
        .await
        else {
            return tools::Outcome::err("tui closed awaiting approval");
        };
        match decision {
            ApprovalDecision::Deny => {
                return tools::Outcome::err(format!("command denied by user ({reason})"));
            }
            // The user looked at this egress shape and said yes: events
            // it so the same kind passes for the rest of the session.
            ApprovalDecision::AlwaysSession => {
                if let (Some(kind), Some(writer)) = (taint_kind, events.as_mut()) {
                    crate::agent::trust::ack_egress(writer, kind);
                }
                always_allow.push(Grant::from_command(&command, reason));
                // an approved outside write opens its directory too, so the
                // follow-up file operations there stop asking
                if let Some(dir) = outside_target.take() {
                    ctx.outside_roots.push(dir.clone());
                    ctx.outside_granted.push(dir);
                }
            }
            ApprovalDecision::RunOnce => {
                if let (Some(kind), Some(writer)) = (taint_kind, events.as_mut()) {
                    crate::agent::trust::ack_egress(writer, kind);
                }
            }
        }
    }

    // checkpoint before running: dangerous-approved commands always;
    // otherwise only if the tree moved since this chain's last snapshot
    // (hash-gate). The classifier cannot see mutations, only risk:
    // formatters and checkout-class commands look safe and mutate
    // silently, and must not run uninsured.
    // bash is the one case whose targets cannot be known in
    // advance, so this is where layer 2 earns its existence.
    let snapshot_wanted = needs_approval.is_some()
        || checkpoints::tree_changed(&ctx.root, ctx.shadow_store, ctx.checkpoint_chain());
    let mut bash_sha: Option<String> = None;
    if snapshot_wanted
        && let Ok(Some(sha)) = checkpoints::snapshot_session(
            &ctx.root,
            ctx.shadow_store,
            ctx.checkpoint_chain(),
            &format!("pre_bash {command}"),
        )
    {
        ctx.checkpoints
            .push((sha.clone(), format!("bash {command}")));
        bash_sha = Some(sha);
    }

    // 2. run it through the normal bash handler on a blocking thread so a long
    //    command never stalls the async runtime (and the TUI render loop)
    let mut outcome = run_tool_blocking(ctx, "bash", &call.args).await;

    // 3. OpenCode-style `before -> after` bash attribution: if a pre-bash
    //    snapshot was taken (or exists on the chain when an event log is active)
    //    and the command modified files, record their pre-images and post-bash
    //    hashes so `/undo` can revert bash writes (including `mv`/`rm`) while
    //    protecting later user edits.
    if let Some(sha) = bash_sha {
        let diffs = checkpoints::bash_file_diffs(&ctx.root, ctx.shadow_store, &sha);
        if !diffs.is_empty() {
            outcome = outcome.with_file_diffs(diffs);
        }
    }
    outcome
}

/// The approval dialog itself: show the subject, await one answer, and
/// events the decision — a live session's approvals were invisible in
/// the event log, which made "why was the agent allowed to do that"
/// unanswerable after the fact. `None` means the channel died (tui
/// closed or agent cancelled).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn approval_dialog(
    subject: String,
    reason: &str,
    grant: String,
    allow_always: bool,
    tx: &mpsc::Sender<AgentEvent>,
    ctl: &mut mpsc::Receiver<ControlMsg>,
    next_id: &mut u64,
    events: &mut Option<crate::agent::events::EventLog>,
) -> Option<ApprovalDecision> {
    let id = *next_id;
    *next_id += 1;
    tx.send(AgentEvent::Approval {
        id,
        subject: subject.clone(),
        reason: reason.to_string(),
        grant,
        allow_always,
    })
    .await
    .ok()?;
    let decision = loop {
        match ctl.recv().await {
            Some(ControlMsg::ApprovalAnswer { id: aid, decision }) if aid == id => break decision,
            Some(_) => continue,
            None => return None,
        }
    };
    if let Some(writer) = events.as_mut() {
        let _ = writer.append(
            "approval",
            serde_json::json!({
                "subject": subject.chars().take(200).collect::<String>(),
                "reason": reason,
                "decision": decision.as_str(),
            }),
        );
    }
    Some(decision)
}

/// One gate for every mutating tool call — the workspace boundary and ask
/// mode's blanket confirm together:
///
/// - a file tool writing outside the project root asks (full access and a
///   session grant run it); a confirmed call carries a one-shot
///   `outside_write_permit` that opens the jail for exactly that call
/// - ask mode additionally confirms every other mutating call
///
/// `None` means run the call; `Some` is the final, refused outcome. An
/// AlwaysSession answer grants that file for the session: the next write or
/// edit of the same path is exactly what the user said yes to, and nothing
/// else is.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn mutating_tool_gate(
    call: &ToolCallReq,
    ctx: &mut ToolCtx,
    tx: &mpsc::Sender<AgentEvent>,
    ctl: &mut mpsc::Receiver<ControlMsg>,
    next_id: &mut u64,
    mode: ApprovalMode,
    file_grants: &mut Vec<String>,
    events: &mut Option<crate::agent::events::EventLog>,
) -> Option<tools::Outcome> {
    let name = call.name.as_str();
    let path = tools::call_path(name, &call.args);
    let rel = path.as_deref().and_then(|p| ctx.resolve_scope_rel(p).ok());
    let granted = path
        .as_deref()
        .is_some_and(|p| ctx.outside_granted_covers(p))
        || rel
            .as_ref()
            .is_some_and(|rel| file_grants.iter().any(|granted| granted == rel));
    // the workspace boundary, asked as an approval rather than refused:
    // one yes opens the target directory for the run (or the session)
    if matches!(name, "write" | "edit")
        && let Some(path) = path.as_deref()
        && ctx.outside_uncovered(path)
    {
        // full access and already-granted directories resolve on their own
        // (outside_all / outside_roots); there is nothing to ask
        if mode == ApprovalMode::Full {
            return None;
        }
        let dir = ctx
            .outside_grant_dir(path)
            .map(|d| d.to_string_lossy().to_string())
            .unwrap_or_default();
        let decision = approval_dialog(
            mutating_subject(name, &call.args),
            OUTSIDE_WRITE_REASON,
            format!("{dir}/**"),
            true,
            tx,
            ctl,
            next_id,
            events,
        )
        .await;
        match decision {
            Some(ApprovalDecision::Deny) => {
                return Some(tools::Outcome::err(format!(
                    "{name} denied by user ({OUTSIDE_WRITE_REASON})"
                )));
            }
            Some(ApprovalDecision::AlwaysSession) => {
                if let Some(d) = ctx.outside_grant_dir(path) {
                    ctx.outside_roots.push(d.clone());
                    ctx.outside_granted.push(d);
                }
            }
            Some(ApprovalDecision::RunOnce) => {
                if let Some(d) = ctx.outside_grant_dir(path) {
                    ctx.outside_roots.push(d);
                }
            }
            None => return Some(tools::Outcome::err("tui closed awaiting approval")),
        }
        // this exact call was just confirmed — ask mode has nothing to add
        return None;
    }
    if mode == ApprovalMode::Ask && !granted {
        let subject = mutating_subject(name, &call.args);
        let grant = rel.clone().unwrap_or_default();
        let decision = approval_dialog(
            subject,
            ASK_MODE_REASON,
            grant,
            rel.is_some(),
            tx,
            ctl,
            next_id,
            events,
        )
        .await;
        match decision {
            Some(ApprovalDecision::Deny) => {
                return Some(tools::Outcome::err(format!(
                    "{name} call denied by user (ask mode)"
                )));
            }
            Some(ApprovalDecision::AlwaysSession) => {
                if let Some(rel) = rel {
                    file_grants.push(rel);
                }
            }
            Some(ApprovalDecision::RunOnce) => {}
            None => return Some(tools::Outcome::err("tui closed awaiting approval")),
        }
    }
    None
}

/// The subject of a network approval: the tool and what it will reach.
pub(crate) fn network_subject(name: &str, args: &serde_json::Value) -> String {
    let what = if name == "webfetch" {
        args["url"].as_str()
    } else {
        args["query"].as_str()
    }
    .unwrap_or("");
    if what.is_empty() {
        name.to_string()
    } else {
        format!("{name} {what}")
    }
}

/// ASK confirms every network read; there is no grant to earn because the
/// user chose to see each call. `None` means run it, `Some` is the refusal.
pub(crate) async fn network_read_gate(
    subject: String,
    tx: &mpsc::Sender<AgentEvent>,
    ctl: &mut mpsc::Receiver<ControlMsg>,
    next_id: &mut u64,
    events: &mut Option<crate::agent::events::EventLog>,
) -> Option<tools::Outcome> {
    match approval_dialog(
        subject,
        NETWORK_ASK_REASON,
        String::new(),
        false,
        tx,
        ctl,
        next_id,
        events,
    )
    .await
    {
        Some(ApprovalDecision::Deny) => Some(tools::Outcome::err(
            "network read denied by user (ask mode)",
        )),
        Some(_) => None,
        None => Some(tools::Outcome::err("tui closed awaiting approval")),
    }
}

/// A short, human-readable line for a write/edit approval: the path, the
/// line delta or size, and the first content line. Never the whole payload —
/// the dialog stays a decision, not a diff viewer.
fn mutating_subject(name: &str, args: &serde_json::Value) -> String {
    let path = tools::call_path(name, args).unwrap_or_default();
    let mut subject = if path.is_empty() {
        name.to_string()
    } else {
        format!("{name} {path}")
    };
    match name {
        "edit" => {
            let old = args["old_string"].as_str().unwrap_or("");
            let new = args["new_string"].as_str().unwrap_or("");
            let all = args["replace_all"].as_bool().unwrap_or(false);
            subject.push_str(&format!(
                " (+{} −{} lines{})",
                new.lines().count(),
                old.lines().count(),
                if all { ", replace all" } else { "" }
            ));
            if let Some(line) = first_content_line(new) {
                subject.push_str(&format!(" · \"{line}\""));
            }
        }
        "write" => {
            let content = args["content"].as_str().unwrap_or("");
            subject.push_str(&format!(
                " ({} lines, {})",
                content.lines().count(),
                short_bytes(content.len())
            ));
            if let Some(line) = first_content_line(content) {
                subject.push_str(&format!(" · \"{line}\""));
            }
        }
        _ => {
            let summary = tools::call_summary(name, args);
            if !summary.is_empty() {
                subject = format!("{name} {summary}");
            }
        }
    }
    subject
}

fn first_content_line(text: &str) -> Option<String> {
    let line = text.lines().find(|line| !line.trim().is_empty())?;
    let flat = line.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut clipped: String = flat.chars().take(60).collect();
    if flat.chars().count() > 60 {
        clipped.push('…');
    }
    Some(clipped)
}

fn short_bytes(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

/// Every path a read-only call points at: `read` takes one `file_path` or up to
/// sixteen fused `paths`, `ls`/`outline`/`grep` take a `path`. `glob` patterns
/// are not paths and stay out.
///
/// The plural is the whole point. `call_path` answers for `file_path` only, so a
/// fused read had no path for the outside gate to look at: the call skipped the
/// dialog and the handler refused its outside targets — a refusal where the mode
/// promises a question.
pub(crate) fn read_call_paths(name: &str, args: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |value: Option<&str>| {
        if let Some(path) = value.map(str::trim).filter(|s| !s.is_empty()) {
            out.push(path.to_string());
        }
    };
    match name {
        "read" => {
            push(args["file_path"].as_str());
            if let Some(paths) = args["paths"].as_array() {
                for path in paths {
                    push(path.as_str());
                }
            }
        }
        "ls" | "outline" | "grep" => push(args["path"].as_str()),
        _ => {}
    }
    out
}

/// Whether this read call touches anything outside what the run may already
/// read. True for a fused read with one outside path among ten inside ones:
/// that one path is what the dialog is about.
pub(crate) fn reads_outside(ctx: &ToolCtx, name: &str, args: &serde_json::Value) -> bool {
    read_call_paths(name, args)
        .iter()
        .any(|path| ctx.outside_uncovered(path))
}

/// Execute a tool handler on a dedicated blocking thread.
///
/// `tools::execute` can run for a long time (e.g. `bash` up to its timeout), and
/// calling it directly inside this async task would occupy a tokio worker —
/// which, when the scheduler places `run_agent` on the `block_on` driver thread,
/// freezes the TUI for the whole call. `spawn_blocking` uses a separate thread
/// pool, so the runtime (and the UI) stay responsive. The cloned `ToolCtx` is
/// moved in and its events/checkpoint bookkeeping is merged back afterwards so
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
    let checkpoints = std::mem::take(&mut ctx.checkpoints);
    let fallback = (
        files_read.clone(),
        read_windows.clone(),
        checkpoints.clone(),
    );
    let mut exec_ctx = ctx.clone();
    exec_ctx.files_read = files_read;
    exec_ctx.read_windows = read_windows;
    exec_ctx.checkpoints = checkpoints;
    let name = name.to_string();
    let args = args.clone();
    let joined = tokio::task::spawn_blocking(move || {
        let outcome = tools::execute(&mut exec_ctx, &name, &args);
        (outcome, exec_ctx)
    })
    .await;
    match joined {
        Ok((outcome, worker)) => {
            ctx.checkpoints = worker.checkpoints;
            ctx.files_read = worker.files_read;
            ctx.read_windows = worker.read_windows;
            ctx.activated_skills = worker.activated_skills;
            outcome
        }
        Err(e) => {
            let (files_read, read_windows, checkpoints) = fallback;
            ctx.checkpoints = checkpoints;
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
    use tokio::sync::mpsc;

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

    #[test]
    fn write_and_edit_approvals_get_a_short_summary() {
        let edit = mutating_subject(
            "edit",
            &json!({
                "file_path": "src/a.rs",
                "old_string": "a\nb\n",
                "new_string": "a\nc\n",
            }),
        );
        assert!(edit.contains("edit src/a.rs"), "{edit}");
        assert!(edit.contains("+2 −2 lines"), "{edit}");
        assert!(edit.contains("\"a\""), "{edit}");

        let write = mutating_subject(
            "write",
            &json!({"file_path": "src/b.rs", "content": "fn main() {}\n"}),
        );
        assert!(write.contains("write src/b.rs"), "{write}");
        assert!(write.contains("1 lines"), "{write}");
        assert!(write.contains("\"fn main() {}\""), "{write}");
    }

    #[tokio::test]
    async fn network_ask_gate_has_no_always_and_can_deny() {
        let (tx, mut rx) = mpsc::channel(8);
        let (ctl_tx, mut ctl_rx) = mpsc::channel(8);
        let mut next_id = 0;
        let mut events = None;
        let subject = "webfetch https://example.com".to_string();
        let gate = network_read_gate(subject, &tx, &mut ctl_rx, &mut next_id, &mut events);
        let answer = async {
            let Some(AgentEvent::Approval {
                id,
                reason,
                allow_always,
                ..
            }) = rx.recv().await
            else {
                panic!("expected an approval event");
            };
            assert!(!allow_always, "network approvals must not offer always");
            assert!(reason.contains("network"), "{reason}");
            ctl_tx
                .send(ControlMsg::ApprovalAnswer {
                    id,
                    decision: ApprovalDecision::Deny,
                })
                .await
                .unwrap();
        };
        let (outcome, _) = tokio::join!(gate, answer);
        let outcome = outcome.expect("denied network read");
        assert!(!outcome.ok);
    }

    #[tokio::test]
    async fn ask_file_grant_covers_only_the_approved_file() {
        use crate::agent::tools::ToolCtx;
        use crate::config::ApprovalMode;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "x").unwrap();
        let mut ctx = ToolCtx::new(dir.path());
        let mut grants = Vec::new();
        let mut next_id = 0;
        let mut events = None;

        let call = ToolCallReq::new(
            "c1",
            "edit",
            json!({"file_path": "a.rs", "old_string": "x", "new_string": "y"}),
        );
        let (tx, mut rx) = mpsc::channel(8);
        let (ctl_tx, mut ctl_rx) = mpsc::channel(8);
        let gate = mutating_tool_gate(
            &call,
            &mut ctx,
            &tx,
            &mut ctl_rx,
            &mut next_id,
            ApprovalMode::Ask,
            &mut grants,
            &mut events,
        );
        let answer = async {
            let Some(AgentEvent::Approval { id, grant, .. }) = rx.recv().await else {
                panic!("expected an approval event");
            };
            assert_eq!(grant, "a.rs");
            ctl_tx
                .send(ControlMsg::ApprovalAnswer {
                    id,
                    decision: ApprovalDecision::AlwaysSession,
                })
                .await
                .unwrap();
        };
        let (outcome, _) = tokio::join!(gate, answer);
        assert!(outcome.is_none(), "the first edit must run after approval");
        assert_eq!(grants, vec!["a.rs".to_string()]);

        let again = ToolCallReq::new(
            "c2",
            "edit",
            json!({"file_path": "a.rs", "old_string": "y", "new_string": "z"}),
        );
        let outcome = mutating_tool_gate(
            &again,
            &mut ctx,
            &tx,
            &mut ctl_rx,
            &mut next_id,
            ApprovalMode::Ask,
            &mut grants,
            &mut events,
        )
        .await;
        assert!(outcome.is_none(), "the granted file must not ask again");
        assert!(
            rx.try_recv().is_err(),
            "no second approval for the granted file"
        );

        // the grant is file-scoped, not tool-scoped: a write to the same
        // path is covered too
        let same_file_write =
            ToolCallReq::new("c2b", "write", json!({"file_path": "a.rs", "content": "z"}));
        let outcome = mutating_tool_gate(
            &same_file_write,
            &mut ctx,
            &tx,
            &mut ctl_rx,
            &mut next_id,
            ApprovalMode::Ask,
            &mut grants,
            &mut events,
        )
        .await;
        assert!(outcome.is_none(), "the granted file must cover writes too");
        assert!(rx.try_recv().is_err());

        let other = ToolCallReq::new(
            "c3",
            "write",
            json!({"file_path": "b.rs", "content": "new"}),
        );
        let gate = mutating_tool_gate(
            &other,
            &mut ctx,
            &tx,
            &mut ctl_rx,
            &mut next_id,
            ApprovalMode::Ask,
            &mut grants,
            &mut events,
        );
        let answer = async {
            let Some(AgentEvent::Approval { id, .. }) = rx.recv().await else {
                panic!("a different file must still ask");
            };
            ctl_tx
                .send(ControlMsg::ApprovalAnswer {
                    id,
                    decision: ApprovalDecision::Deny,
                })
                .await
                .unwrap();
        };
        let (outcome, _) = tokio::join!(gate, answer);
        assert!(outcome.is_some_and(|o| !o.ok));
    }

    /// A fused `read` carries up to sixteen paths and no `file_path`, so the
    /// gate saw no path at all: the call skipped the dialog and the handler
    /// refused its outside targets — a refusal in the one mode that asks.
    #[test]
    fn a_fused_read_names_every_path_for_the_outside_gate() {
        use crate::agent::tools::ToolCtx;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "x").unwrap();
        let far = outside.path().join("b.rs");
        std::fs::write(&far, "y").unwrap();
        let ctx = ToolCtx::new(root.path());

        let fused = json!({"paths": ["a.rs", far.to_string_lossy()]});
        assert_eq!(
            read_call_paths("read", &fused).len(),
            2,
            "every fused path is a candidate, not just the first"
        );
        assert!(
            reads_outside(&ctx, "read", &fused),
            "one outside target in a fused call is enough to ask"
        );
        assert!(!reads_outside(&ctx, "read", &json!({"paths": ["a.rs"]})));
        assert!(!reads_outside(&ctx, "read", &json!({"file_path": "a.rs"})));
        assert!(
            !reads_outside(&ctx, "glob", &json!({"pattern": "**/*.rs"})),
            "a pattern is not a path"
        );
    }

    /// The dialog for an outside write names its grant `dir/**`. Until the grant
    /// covered the directory, every next file inside it earned its own dialog, so
    /// the answer the user gave ("always") meant less than the text offered.
    #[tokio::test]
    async fn always_on_an_outside_directory_stops_asking_per_file() {
        use crate::agent::tools::ToolCtx;
        use crate::config::ApprovalMode;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let first = outside.path().join("first.rs");
        let second = outside.path().join("second.rs");
        std::fs::write(&first, "x").unwrap();
        std::fs::write(&second, "x").unwrap();

        let mut ctx = ToolCtx::new(root.path());
        let mut grants = Vec::new();
        let mut next_id = 0;
        let mut events = None;
        let (tx, mut rx) = mpsc::channel(8);
        let (ctl_tx, mut ctl_rx) = mpsc::channel(8);
        let write = |path: &std::path::Path, id: &str| {
            ToolCallReq::new(
                id,
                "write",
                json!({"file_path": path.to_string_lossy(), "content": "y"}),
            )
        };

        let call = write(&first, "c1");
        let gate = mutating_tool_gate(
            &call,
            &mut ctx,
            &tx,
            &mut ctl_rx,
            &mut next_id,
            ApprovalMode::Ask,
            &mut grants,
            &mut events,
        );
        let answer = async {
            let Some(AgentEvent::Approval { id, grant, .. }) = rx.recv().await else {
                panic!("the first write outside the root must ask");
            };
            assert!(
                grant.ends_with("/**"),
                "the dialog offers a directory, and the answer must mean that: {grant}"
            );
            ctl_tx
                .send(ControlMsg::ApprovalAnswer {
                    id,
                    decision: ApprovalDecision::AlwaysSession,
                })
                .await
                .unwrap();
        };
        let (outcome, _) = tokio::join!(gate, answer);
        assert!(outcome.is_none(), "the approved call runs");

        let again = write(&second, "c2");
        let outcome = mutating_tool_gate(
            &again,
            &mut ctx,
            &tx,
            &mut ctl_rx,
            &mut next_id,
            ApprovalMode::Ask,
            &mut grants,
            &mut events,
        )
        .await;
        assert!(
            outcome.is_none(),
            "a second file in a directory the session was given for must not ask again"
        );
        assert!(
            rx.try_recv().is_err(),
            "and no dialog may be queued behind the call"
        );
    }

    /// The whole point of AUTO is that the classifier decides, so the journal has
    /// to say what it decided — including, especially, the commands it let
    /// through without asking. No dialog means no `approval` record, and until
    /// this record existed there was no evidence at all of the judgment.
    #[tokio::test]
    async fn auto_records_the_verdict_for_a_command_it_never_asks_about() {
        use crate::config::ApprovalMode;

        let dir = tempfile::tempdir().unwrap();
        let mut ctx = crate::agent::tools::ToolCtx::new(dir.path())
            .in_session("verdict-record".to_string());
        let call = ToolCallReq::new("c1", "bash", json!({"command": "echo classifier-floor"}));
        let (tx, _rx) = mpsc::channel(8);
        let (_ctl_tx, mut ctl_rx) = mpsc::channel(8);
        // No user on the other end: a command that needed a dialog would come
        // back refused here, so an ok outcome proves the pass was silent.
        drop(_ctl_tx);
        let mut events = crate::agent::events::EventLog::open(dir.path(), "verdict-record").ok();
        let mut outside = None;
        let outcome = bash_call(
            &call,
            &mut ctx,
            &tx,
            &mut ctl_rx,
            &mut Vec::new(),
            &[],
            &mut 0u64,
            0,
            ApprovalMode::Auto,
            &mut events,
            &mut outside,
        )
        .await;
        assert!(outcome.ok, "a safe command runs: {}", outcome.output);

        let records = crate::agent::events::EventLog::records(dir.path()).expect("read journal");
        let verdict = records
            .iter()
            .find(|r| r.kind == "verdict")
            .expect("a silent pass must still record the classifier's verdict");
        assert_eq!(verdict.fields["verdict"], "safe");
        assert_eq!(verdict.fields["mode"], "auto");
        assert_eq!(verdict.fields["call_id"], "c1");
        assert!(
            verdict.fields["command"]
                .as_str()
                .is_some_and(|c| c.contains("classifier-floor")),
            "the record names what was judged: {:?}",
            verdict.fields["command"]
        );
    }
}
