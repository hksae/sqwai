use super::{AgentEvent, AgentHandle, AgentInput, ApprovalDecision, ControlMsg, FallbackCandidate, spawn_agent};
use crate::agent::tools;
use crate::config::EffortLevel;
use crate::plan;
use crate::providers::{Message, Role, SharedProvider, SystemPart, ToolCallReq};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;


const MAX_SUBAGENTS_PER_CALL: usize = 8;
pub(crate) const MAX_PARALLEL_SUBAGENTS: usize = 4;
/// A child that produces nothing in this long is stuck (provider retry
/// loops run far longer): cancel cooperatively, wait out a short grace,
/// then tear the turn down. Without this a hung child stalls the parent
/// turn forever — Esc only stops what comes *after* running children.
pub(crate) const SUBAGENT_TIMEOUT_SECS: u64 = 600;
const SUBAGENT_CANCEL_GRACE_SECS: u64 = 5;

/// One task's text from any shape the model may send: a bare string, or
/// an object carrying it under `task`/`prompt`/`text`/`description` (the
/// last is Claude-Code convention for the short label — better than
/// refusing the whole batch). Trims; empty means absent.
fn subagent_task_text(item: &serde_json::Value) -> Option<String> {
    match item {
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        serde_json::Value::Object(map) => ["task", "prompt", "text", "description"]
            .into_iter()
            .filter_map(|key| map.get(key)?.as_str())
            .map(str::trim)
            .find(|text| !text.is_empty())
            .map(str::to_string),
        _ => None,
    }
}

/// One parsed child task. Children are read-only by default; `write` with
/// `paths` declares a writer scoped to those roots. Paths are normalized
/// here so spawn-time overlap checks and execute-time enforcement agree.
#[derive(Debug, PartialEq)]
pub(crate) struct SubagentTask {
    pub(crate) label: String,
    pub(crate) write: bool,
    pub(crate) paths: Vec<String>,
}

fn subagent_task_spec(item: &serde_json::Value) -> Option<SubagentTask> {
    let label = subagent_task_text(item)?;
    let (write, paths) = match item {
        serde_json::Value::Object(map) => {
            let write = map.get("write").and_then(|v| v.as_bool()).unwrap_or(false);
            let paths = map
                .get("paths")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .map(|p| {
                            p.replace('\\', "/")
                                .trim_start_matches("./")
                                .trim_end_matches('/')
                                .to_string()
                        })
                        .filter(|p| !p.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            (write, paths)
        }
        _ => (false, Vec::new()),
    };
    Some(SubagentTask {
        label,
        write,
        paths,
    })
}

/// Two writer scopes overlap when a path is equal or nested either way.
/// Sibling writers must not overlap: concurrent writes to one file tangle
/// attribution and undo beyond what epochs can separate.
fn scopes_overlap(a: &[String], b: &[String]) -> bool {
    a.iter().any(|x| {
        b.iter().any(|y| {
            x == y || x.starts_with(&format!("{y}/")) || y.starts_with(&format!("{x}/"))
        })
    })
}

pub(crate) fn subagent_tasks_from_args(args: &serde_json::Value) -> Result<Vec<SubagentTask>, String> {
    let mut tasks: Vec<SubagentTask> = match args.get("tasks") {
        Some(serde_json::Value::Array(items)) => {
            items.iter().filter_map(subagent_task_spec).collect()
        }
        // a lone string is one task, not a malformed array
        Some(single) => subagent_task_spec(single).into_iter().collect(),
        None => Vec::new(),
    };
    if tasks.is_empty()
        && let Some(task) = args.get("task").and_then(subagent_task_spec)
    {
        tasks.push(task);
    }
    if tasks.is_empty() {
        return Err(
            "subagent task is required: pass task (string) or tasks (array of strings or objects with task|prompt)"
                .into(),
        );
    }
    if tasks.len() > MAX_SUBAGENTS_PER_CALL {
        return Err(format!(
            "too many subagents: maximum is {MAX_SUBAGENTS_PER_CALL}"
        ));
    }
    // writers declare their scope up front; overlapping siblings refuse
    // before anything spawns, naming both sides.
    for task in &tasks {
        if task.write && task.paths.is_empty() {
            return Err(format!(
                "subagent write needs paths: task '{}' declares write without a scope — name the roots it may touch",
                task.label.chars().take(80).collect::<String>()
            ));
        }
    }
    for (i, a) in tasks.iter().enumerate() {
        if !a.write {
            continue;
        }
        for b in tasks.iter().skip(i + 1) {
            if b.write && scopes_overlap(&a.paths, &b.paths) {
                return Err(format!(
                    "subagent writer scopes overlap: '{}' and '{}' share paths — split the scopes or serialize the work",
                    a.label.chars().take(60).collect::<String>(),
                    b.label.chars().take(60).collect::<String>()
                ));
            }
        }
    }
    Ok(tasks)
}

/// Run a pure-subagent batch concurrently: every call gets its pre-phase
/// (cancel check, journal row, ToolStart row) in call order, the children
/// run overlapped, and results are processed back in call order, so rows,
/// journal and transcript look exactly like a fast sequential batch — only
/// the waits overlap. Mixed batches keep the sequential loop: interleaving
/// arbitrary tools would tangle journal attribution and mutation order.
///
/// Esc semantics match one running subagent: the wait loop polls the
/// parent flag, so a stop request cooperatively stops the children (each
/// gets its own cancel first) instead of waiting out the timeout.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_subagent_batch(
    calls: &[ToolCallReq],
    session_id: &str,
    root: &Path,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    journal: &mut Option<crate::agent::journal::Journal>,
    tx: &mpsc::Sender<AgentEvent>,
    current_step: Option<String>,
    provider: &SharedProvider,
    model_id: &str,
    blocked_patterns: &[String],
    plan_mode: bool,
    context_limit: u64,
    effort: Option<EffortLevel>,
    effort_support: crate::config::EffortSupport,
    max_tokens: Option<u32>,
    system: &[SystemPart],
    mcp: &crate::config::McpConfig,
    lsp: &crate::config::LspConfig,
    read_only: bool,
    shadow_store: crate::config::ShadowStore,
    messages: &mut Vec<Message>,
    diary: crate::config::DiaryConfig,
    memory: crate::config::MemoryConfig,
    compaction: crate::config::CompactionConfig,
    plan_limits: crate::config::PlanConfig,
    fallback_chain: Vec<FallbackCandidate>,
    timeout: std::time::Duration,
) -> bool {
    use futures::{StreamExt, stream};
    // pre-phase in order; stops at the first pre-cancelled call exactly
    // like the sequential loop (that call still gets its cancelled row,
    // later ones only their protocol tool_result)
    let mut dispatch: Vec<(usize, ToolCallReq, std::time::Instant)> = Vec::new();
    let mut cutoff = calls.len();
    let mut interrupted = false;
    for (index, call) in calls.iter().enumerate() {
        let pre_cancelled = cancel.swap(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(writer) = journal.as_mut() {
            let active = plan::open_active_for_session(root, Some(session_id))
                .ok()
                .flatten();
            let plan_id = active.as_ref().map(|p| p.id.clone());
            let step = current_step.clone().or_else(|| {
                active.as_ref().and_then(|p| {
                    p.steps
                        .iter()
                        .find(|s| s.status == plan::StepStatus::InProgress)
                        .map(|s| s.id.clone())
                })
            });
            writer.set_attribution(step, plan_id, "main");
            let _ = writer.append(
                "tool_call",
                serde_json::json!({
                    "tool": call.name,
                    "call_id": call.id,
                    "args_digest": tools::call_summary(&call.name, &call.args),
                    "path": tools::call_path(&call.name, &call.args),
                }),
            );
        }
        let _ = tx
            .send(AgentEvent::ToolStart {
                name: call.name.clone(),
                summary: tools::call_summary(&call.name, &call.args),
                call_id: call.id.clone(),
            })
            .await;
        if pre_cancelled {
            cutoff = index;
            break;
        }
        dispatch.push((index, call.clone(), std::time::Instant::now()));
    }
    // fan out; results collected out of order, processed back in order
    let mut done: Vec<(usize, ToolCallReq, std::time::Instant, tools::Outcome)> =
        stream::iter(dispatch)
            .map(|(index, call, started)| {
                let session_id = session_id.to_string();
                let provider = provider.clone();
                let model_id = model_id.to_string();
                let root = root.to_path_buf();
                let blocked_patterns = blocked_patterns.to_vec();
                let system = system.to_vec();
                let mcp = mcp.clone();
                let lsp = lsp.clone();
                let diary = diary.clone();
                let memory = memory.clone();
                let compaction = compaction.clone();
                let fallback_chain = fallback_chain.clone();
                let parent_cancel = cancel.clone();
                async move {
                    let outcome = run_subagent(
                        &call,
                        &session_id,
                        tx,
                        &parent_cancel,
                        &provider,
                        &model_id,
                        &root,
                        &blocked_patterns,
                        plan_mode,
                        context_limit,
                        effort,
                        effort_support,
                        max_tokens,
                        system,
                        mcp,
                        lsp,
                        read_only,
                        shadow_store,
                        diary,
                        memory,
                        compaction,
                        plan_limits,
                        fallback_chain,
                        timeout,
                    )
                    .await;
                    (index, call, started, outcome)
                }
            })
            .buffer_unordered(MAX_PARALLEL_SUBAGENTS)
            .collect()
            .await;
    done.sort_by_key(|(index, _, _, _)| *index);
    for (_index, call, tool_started, outcome) in done {
        let _ = tx
            .send(AgentEvent::ToolNotice {
                name: call.name.clone(),
                summary: outcome.output.clone(),
                ok: outcome.ok,
                diff: outcome.diff.clone(),
                call_id: call.id.clone(),
            })
            .await;
        if let Some(writer) = journal.as_mut() {
            let _ = writer
                .append_evidence(
                    "tool_result",
                    serde_json::json!({
                        "tool": call.name,
                        "call_id": call.id,
                        "ok": outcome.ok,
                        "duration_ms": tool_started.elapsed().as_millis(),
                        "summary": outcome.output.chars().take(200).collect::<String>(),
                        "trust": "high",
                        "code": if outcome.cancelled { Some("cancelled") } else { None },
                    }),
                )
                .ok();
        }
        if outcome.cancelled {
            interrupted = true;
        }
        messages.push(Message::tool_result(&call.id, outcome.output, !outcome.ok));
    }
    if cutoff < calls.len() {
        // the call that saw the flag: a cancelled result row, exactly like
        // the sequential loop — then plain protocol results for the rest
        let call = &calls[cutoff];
        let outcome = tools::Outcome::cancelled();
        let _ = tx
            .send(AgentEvent::ToolNotice {
                name: call.name.clone(),
                summary: outcome.output.clone(),
                ok: outcome.ok,
                diff: outcome.diff.clone(),
                call_id: call.id.clone(),
            })
            .await;
        if let Some(writer) = journal.as_mut() {
            let _ = writer
                .append_evidence(
                    "tool_result",
                    serde_json::json!({
                        "tool": call.name,
                        "call_id": call.id,
                        "ok": outcome.ok,
                        "duration_ms": 0,
                        "summary": outcome.output.chars().take(200).collect::<String>(),
                        "trust": "high",
                        "code": Some("cancelled"),
                    }),
                )
                .ok();
        }
        interrupted = true;
        messages.push(Message::tool_result(&call.id, outcome.output, !outcome.ok));
        for rest in &calls[cutoff + 1..] {
            messages.push(Message::tool_result(
                &rest.id,
                "cancelled by user — this tool call was not run".to_string(),
                true,
            ));
        }
    }
    interrupted
}

/// The step a session should hold: whatever the plan keeps in progress,
/// or idle. Used at startup (adopt the interrupted step after a crash,
/// §3.4) and after a subagent returns (the child may have retired or
/// moved the held step, §2.2.3). `ctx.current_step` stays the single live
/// copy; the TUI persists it via `StepCurrent` events.
pub(crate) fn adopt_in_progress_step(root: &Path, session_id: &str) -> Option<String> {
    plan::open_active_for_session(root, Some(session_id))
        .ok()
        .flatten()
        .and_then(|plan| {
            plan.steps
                .into_iter()
                .find(|step| step.status == plan::StepStatus::InProgress)
                .map(|step| step.id)
        })
}

#[allow(clippy::too_many_arguments)] // all parameters are required for subagent configuration
pub(crate) async fn run_subagent(
    call: &ToolCallReq,
    parent_session: &str,
    parent_tx: &mpsc::Sender<AgentEvent>,
    parent_cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    provider: &SharedProvider,
    model_id: &str,
    root: &Path,
    blocked_patterns: &[String],
    plan_mode: bool,
    context_limit: u64,
    effort: Option<EffortLevel>,
    effort_support: crate::config::EffortSupport,
    max_tokens: Option<u32>,
    system: Vec<SystemPart>,
    mcp: crate::config::McpConfig,
    lsp: crate::config::LspConfig,
    read_only: bool,
    shadow_store: crate::config::ShadowStore,
    diary: crate::config::DiaryConfig,
    memory: crate::config::MemoryConfig,
    compaction: crate::config::CompactionConfig,
    plan_limits: crate::config::PlanConfig,
    fallback_chain: Vec<FallbackCandidate>,
    timeout: std::time::Duration,
) -> tools::Outcome {
    let tasks = match subagent_tasks_from_args(&call.args) {
        Ok(tasks) => tasks,
        Err(error) => return tools::Outcome::err(error),
    };
    if tasks.len() > 1 {
        use futures::{StreamExt, stream};
        let outcomes = stream::iter(tasks.into_iter().enumerate())
            .map(|(index, task)| {
                let mut one = call.clone();
                // re-parseable single: the recursive call re-derives the
                // same write scope from these flags
                one.args = serde_json::json!({
                    "task": task.label,
                    "write": task.write,
                    "paths": task.paths,
                });
                let system = system.clone();
                let mcp = mcp.clone();
                let lsp = lsp.clone();
                let diary = diary.clone();
                let memory = memory.clone();
                let compaction = compaction.clone();
                let fallback_chain = fallback_chain.clone();
                async move {
                    let outcome = run_subagent(
                        &one,
                        parent_session,
                        parent_tx,
                        parent_cancel,
                        provider,
                        model_id,
                        root,
                        blocked_patterns,
                        plan_mode,
                        context_limit,
                        effort,
                        effort_support,
                        max_tokens,
                        system,
                        mcp,
                        lsp,
                        read_only,
                        shadow_store,
                        diary,
                        memory,
                        compaction,
                        plan_limits,
                        fallback_chain,
                        timeout,
                    )
                    .await;
                    (
                        index,
                        one.args["task"].as_str().unwrap_or_default().to_string(),
                        outcome,
                    )
                }
            })
            .buffer_unordered(MAX_PARALLEL_SUBAGENTS)
            .collect::<Vec<_>>()
            .await;
        let mut outcomes = outcomes;
        outcomes.sort_by_key(|(index, _, _)| *index);
        let all_ok = outcomes.iter().all(|(_, _, outcome)| outcome.ok);
        let output = outcomes
            .into_iter()
            .map(|(_, task, outcome)| format!("## {task}\n{}", outcome.output))
            .collect::<Vec<_>>()
            .join("\n\n");
        return if all_ok {
            tools::Outcome::ok(output)
        } else {
            tools::Outcome::err(output)
        };
    }
    let task = tasks.into_iter().next().unwrap();
    let id = next_subagent_id();
    let child_session = next_subagent_session();
    // Read-only by default: a writer child needs the explicit flag (with
    // paths, validated above) on top of the session lock.
    let child_read_only = read_only || !task.write;
    if task.write {
        tools::register_subagent_scope(&child_session, task.paths.clone());
    }
    // NOTE (§2.2.4): children always complete inside this tool call — the
    // event loop below is awaited before the outcome returns. There is no
    // fire-and-forget spawn, so `plan finish` can never race still-running
    // children of the same step; no extra gate is needed for that.
    // Inherit the spawning step, if any (§2.2.4): the child stamps this
    // context on its records and stops mutating once the epoch moves on.
    // Resolved against the PARENT session, never the global fallback: the
    // most recent active plan may belong to another session, and the child
    // must work its parent's step or none at all.
    let parent_step = plan::open_active_for_session(root, Some(parent_session))
        .ok()
        .flatten()
        .and_then(|plan| {
            plan.steps
                .iter()
                .find(|step| step.status == plan::StepStatus::InProgress)
                .map(|step| plan::StepContext {
                    plan_id: plan.id.clone(),
                    step_id: step.id.clone(),
                    step_epoch: step.step_epoch,
                })
        });
    let _ = parent_tx
        .send(AgentEvent::SubagentStart {
            id,
            task: task.label.clone(),
        })
        .await;
    // #171: the child works its parent's step, so it joins the parent plan
    // explicitly. Without membership its evidence cannot attach under
    // session-strict resolution — and silent fallback is gone on purpose.
    // Serialized: concurrent siblings must not read-modify-write the plan
    // file over each other and drop a join.
    {
        let _guard = PLAN_JOIN_LOCK.lock().await;
        join_plan_session(root, parent_step.as_ref(), parent_session, &child_session);
    }
    let child = spawn_agent(AgentInput {
        provider: provider.clone(),
        model_id: model_id.to_string(),
        model_key: format!("sub-{id}"),
        effort,
        effort_support,
        max_tokens,
        system,
        messages: vec![Message::new(Role::User, task.label.clone())],
        root: root.to_path_buf(),
        // #190: NOT `sub-{id}` — the numeric counter resets on restart
        // and would append to a previous run's journal file
        session_id: child_session.clone(),
        blocked_patterns: blocked_patterns.to_vec(),
        plan_mode,
        context_limit,
        enable_tools: true,
        read_only: child_read_only,
        previous_response_id: None,
        summary: None,
        mcp,
        lsp,
        compact_only: false,
        diary,
        memory,
        compaction,
        plan_limits,
        shadow_store,
        subagent_depth: 1,
        parent_step,
        parent_session: Some(parent_session.to_string()),
        fallback_chain,
    });
    let mut child = child;
    // S1: visible to undo's cancellation signal until it joins. The guard
    // leaves the map on every exit path below, including early returns.
    let _child_slot = crate::agent::undo_guard::track_child(id, child.child_control());
    let mut output = String::new();
    let deadline = tokio::time::Instant::now() + timeout;
    // Esc polling: the parent cancel flag is the only stop signal visible
    // inside this wait — without it a stop request sits unobserved until
    // the timeout, and the TUI shows "cancelling…" forever (§3.7).
    let mut cancel_poll = tokio::time::interval(std::time::Duration::from_millis(100));
    loop {
        // Checked at the top of every iteration, not only on the interval
        // tick. The select below is `biased`, so while the child keeps
        // producing events its `recv` branch is always the ready one and the
        // interval branch is never polled — a fast child streaming a long
        // answer would starve the check and Esc would look ignored. The
        // loop always consumes one event per iteration, so this check can
        // never starve; the interval branch still covers a silent child
        // parked in `recv`.
        if parent_cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return stop_child(&mut child, parent_tx, id, "cancelled by user").await;
        }
        let event = tokio::select! {
            biased;
            res = tokio::time::timeout_at(deadline, child.rx.recv()) => match res {
            Ok(Some(event)) => event,
            Ok(None) => {
                let result = tools::Outcome::err("subagent disconnected");
                let _ = parent_tx
                    .send(AgentEvent::SubagentDone {
                        id,
                        ok: false,
                        output: result.output.clone(),
                    })
                    .await;
                return result;
            }
            Err(_) => {
                return stop_child(&mut child, parent_tx, id, &format!("timed out after {}s", timeout.as_secs())).await;
            }
            },
            _ = cancel_poll.tick() => {
                continue;
            }
        };
        match event {
            AgentEvent::TextDelta(text) => {
                output.push_str(&text);
                let _ = parent_tx.send(AgentEvent::SubagentText { id, text }).await;
            }
            AgentEvent::ThinkingDelta(text) => {
                let _ = parent_tx
                    .send(AgentEvent::SubagentThinking { id, text })
                    .await;
            }
            AgentEvent::ToolStart { name, summary, .. } => {
                let _ = parent_tx
                    .send(AgentEvent::SubagentToolStart { id, name, summary })
                    .await;
            }
            AgentEvent::ToolNotice {
                name,
                summary,
                ok,
                diff,
                ..
            } => {
                let _ = parent_tx
                    .send(AgentEvent::SubagentToolDone {
                        id,
                        name,
                        summary,
                        ok,
                        diff,
                    })
                    .await;
            }
            AgentEvent::Completed(result) => {
                let result = match result {
                    Ok(outcome) => {
                        if output.is_empty() {
                            output = outcome
                                .messages
                                .iter()
                                .rev()
                                .find(|m| m.role == Role::Assistant)
                                .map(|m| m.content.clone())
                                .unwrap_or_default();
                        }
                        tools::Outcome::ok(output.clone())
                    }
                    Err(error) => tools::Outcome::err(error),
                };
                let _ = parent_tx
                    .send(AgentEvent::SubagentDone {
                        id,
                        ok: result.ok,
                        output: result.output.clone(),
                    })
                    .await;
                return result;
            }
            // interactive child events have no parent surface; answer them
            // immediately so the child never blocks on an answer nobody can
            // give (a declined proposal tells it to ask the user directly)
            AgentEvent::AskUser { id, .. } => {
                let _ = child.control.try_send(ControlMsg::AskAnswer {
                    id,
                    text: "subagents cannot reach the user; decide yourself and continue"
                        .to_string(),
                });
            }
            AgentEvent::Approval { id, .. } => {
                let _ = child.control.try_send(ControlMsg::ApprovalAnswer {
                    id,
                    decision: ApprovalDecision::Deny,
                });
            }
            AgentEvent::PlanProposal { id, .. } => {
                let _ = child
                    .control
                    .try_send(ControlMsg::PlanAnswer { id, accept: false });
            }
            _ => {}
        }
    }
}

/// Stop a child that will not stop itself: ask cooperatively first (a
/// `bash` child kills its own process tree on this), wait out a short
/// grace, then tear down hard. Always ends with `SubagentDone(ok:false)`
/// and the error outcome — the parent turn must never hang on a child.
/// `reason` names the trigger ("timed out after Ns" / "cancelled by
/// user"); a child that finishes inside the grace is still discarded: its
/// result belongs to a turn that already moved on.
async fn stop_child(
    child: &mut AgentHandle,
    parent_tx: &mpsc::Sender<AgentEvent>,
    id: u64,
    reason: &str,
) -> tools::Outcome {
    child.request_tool_cancel();
    let grace = std::time::Duration::from_secs(SUBAGENT_CANCEL_GRACE_SECS);
    let finished = tokio::time::timeout(grace, child.rx.recv()).await;
    child.abort();
    let result = if matches!(finished, Ok(Some(AgentEvent::Completed(_)))) {
        tools::Outcome::err(format!(
            "subagent {reason} (finished during cancel, result discarded)"
        ))
    } else {
        tools::Outcome::err(format!("subagent {reason}"))
    };
    let _ = parent_tx
        .send(AgentEvent::SubagentDone {
            id,
            ok: false,
            output: result.output.clone(),
        })
        .await;
    result
}

fn next_subagent_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Serializes plan-membership joins: concurrent sibling subagents share one
/// plan file, and an unlocked read-modify-write would drop all but one join.
static PLAN_JOIN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Join one child session into its parent plan (#171: without membership
/// its evidence cannot attach under session-strict resolution).
/// Journal-first like every plan mutation, in the PARENT's journal: the
/// startup replay only scans the cursor session's suffix, so an intent in
/// the child's file would never replay. The cursor advances, so a crash
/// heals by replay instead of leaving a store write the journal never saw.
fn join_plan_session(
    root: &Path,
    parent_step: Option<&plan::StepContext>,
    parent_session: &str,
    child_session: &str,
) {
    if let Some(step) = parent_step
        && let Ok(plan) = plan::open(root, &step.plan_id)
        && !plan.sessions.iter().any(|s| s == child_session)
    {
        let mut plan = plan;
        // apply first: commit only persists the already-mutated plan behind
        // the journal intent (same order as the plan tool path)
        if plan::apply(
            &mut plan,
            plan::Op::Join {
                session: child_session.to_string(),
            },
            &plan::Limits::default(),
            None,
        )
        .is_ok()
        {
            let _ = plan::commit(
                root,
                parent_session,
                &mut plan,
                "join",
                "host",
                true,
                serde_json::json!({"session": child_session}),
            );
        }
    }
}

/// Journal/shadow identity for a child agent. Unique across process
/// restarts (#190): a bare per-process counter resets to 1, so a resumed
/// session's new subagent would append to the previous run's `sub-1.jsonl`.
/// Wall ms + pid + counter cannot repeat (same pid means same process,
/// where the counter differs).
pub(crate) fn next_subagent_session() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("sub-{ms}-{}-{n}", std::process::id())
}

