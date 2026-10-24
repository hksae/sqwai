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
    journal: &mut Option<crate::agent::journal::Journal>,
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
            tx,
            ctl,
            next_id,
            journal,
        )
        .await
        else {
            return tools::Outcome::err("tui closed awaiting approval");
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
                always_allow.push(Grant::from_command(&command, reason));
                // an approved outside write opens its directory too, so the
                // follow-up file operations there stop asking
                if let Some(dir) = outside_target.take() {
                    ctx.outside_roots.push(dir.clone());
                    ctx.outside_granted.push(dir);
                }
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
        ctx.journal.push((sha.clone(), format!("bash {command}")));
        bash_sha = Some(sha);
    }

    // 2. run it through the normal bash handler on a blocking thread so a long
    //    command never stalls the async runtime (and the TUI render loop)
    let mut outcome = run_tool_blocking(ctx, "bash", &call.args).await;

    // 3. OpenCode-style `before -> after` bash attribution: if a pre-bash
    //    snapshot was taken (or exists on the chain when a journal is active)
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
/// journal the decision — a live session's approvals were invisible in
/// the journal, which made "why was the agent allowed to do that"
/// unanswerable after the fact. `None` means the channel died (tui
/// closed or agent cancelled).
pub(crate) async fn approval_dialog(
    subject: String,
    reason: &str,
    grant: String,
    tx: &mpsc::Sender<AgentEvent>,
    ctl: &mut mpsc::Receiver<ControlMsg>,
    next_id: &mut u64,
    journal: &mut Option<crate::agent::journal::Journal>,
) -> Option<ApprovalDecision> {
    let id = *next_id;
    *next_id += 1;
    tx.send(AgentEvent::Approval {
        id,
        subject: subject.clone(),
        reason: reason.to_string(),
        grant,
    })
    .await
    .ok()?;
    let decision = loop {
        match ctl.recv().await {
            Some(ControlMsg::ApprovalAnswer { id: aid, decision }) if aid == id => {
                break decision
            }
            Some(_) => continue,
            None => return None,
        }
    };
    if let Some(writer) = journal.as_mut() {
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
/// AlwaysSession answer grants the whole tool for the session (`edit *`),
/// which is coarser than a bash grant but honest: the next edit of the
/// same file or a different one is exactly what the user said yes to.
pub(crate) async fn mutating_tool_gate(
    call: &ToolCallReq,
    ctx: &mut ToolCtx,
    tx: &mpsc::Sender<AgentEvent>,
    ctl: &mut mpsc::Receiver<ControlMsg>,
    next_id: &mut u64,
    mode: ApprovalMode,
    tool_grants: &mut Vec<String>,
    journal: &mut Option<crate::agent::journal::Journal>,
) -> Option<tools::Outcome> {
    let name = call.name.as_str();
    let granted = tool_grants.iter().any(|t| t == name);
    // the workspace boundary, asked as an approval rather than refused:
    // one yes opens the target directory for the run (or the session)
    if matches!(name, "write" | "edit" | "multi_edit")
        && let Some(path) = tools::call_path(name, &call.args)
        && ctx.outside_uncovered(&path)
    {
        // full access and already-granted directories resolve on their own
        // (outside_all / outside_roots); there is nothing to ask
        if mode == ApprovalMode::Full {
            return None;
        }
        let dir = ctx
            .outside_grant_dir(&path)
            .map(|d| d.to_string_lossy().to_string())
            .unwrap_or_default();
        let decision = approval_dialog(
            format!("{name} {path}"),
            OUTSIDE_WRITE_REASON,
            format!("{dir}/**"),
            tx,
            ctl,
            next_id,
            journal,
        )
        .await;
        match decision {
            Some(ApprovalDecision::Deny) => {
                return Some(tools::Outcome::err(format!(
                    "{name} denied by user ({OUTSIDE_WRITE_REASON})"
                )))
            }
            Some(ApprovalDecision::AlwaysSession) => {
                if let Some(d) = ctx.outside_grant_dir(&path) {
                    ctx.outside_roots.push(d.clone());
                    ctx.outside_granted.push(d);
                }
            }
            Some(ApprovalDecision::RunOnce) => {
                if let Some(d) = ctx.outside_grant_dir(&path) {
                    ctx.outside_roots.push(d);
                }
            }
            None => return Some(tools::Outcome::err("tui closed awaiting approval")),
        }
        // this exact call was just confirmed — ask mode has nothing to add
        return None;
    }
    if mode == ApprovalMode::Ask && !granted {
        let subject = match tools::call_summary(name, &call.args) {
            summary if summary.is_empty() => name.to_string(),
            summary => format!("{name} {summary}"),
        };
        let decision = approval_dialog(
            subject,
            ASK_MODE_REASON,
            format!("{name} *"),
            tx,
            ctl,
            next_id,
            journal,
        )
        .await;
        match decision {
            Some(ApprovalDecision::Deny) => {
                return Some(tools::Outcome::err(format!(
                    "{name} call denied by user (ask mode)"
                )))
            }
            Some(ApprovalDecision::AlwaysSession) => tool_grants.push(name.to_string()),
            Some(ApprovalDecision::RunOnce) => {}
            None => return Some(tools::Outcome::err("tui closed awaiting approval")),
        }
    }
    None
}

/// The path a read-only tool points at, when it has one: `read`/`ls`/
/// `outline` via `call_path`, `grep` by its own `path` argument. `glob`
/// patterns are not paths and stay out.
pub(crate) fn read_call_path(name: &str, args: &serde_json::Value) -> Option<String> {
    if name == "grep" {
        return args
            .get("path")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
    }
    tools::call_path(name, args)
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
            ctx.activated_skills = worker.activated_skills;
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
