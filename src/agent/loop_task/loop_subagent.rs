use super::{
    AgentEvent, AgentHandle, AgentInput, ApprovalDecision, ControlMsg, FallbackCandidate,
    spawn_agent,
};
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
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SubagentTask {
    pub(crate) label: String,
    pub(crate) write: bool,
    pub(crate) paths: Vec<String>,
    pub(crate) context: Option<String>,
    pub(crate) files: Vec<String>,
}

fn normalize_rel_path_list(arr: &[serde_json::Value]) -> Vec<String> {
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
}

fn subagent_task_spec(item: &serde_json::Value) -> Option<SubagentTask> {
    let label = subagent_task_text(item)?;
    let (write, paths, context, files) = match item {
        serde_json::Value::Object(map) => {
            let write = map.get("write").and_then(|v| v.as_bool()).unwrap_or(false);
            let paths = map
                .get("paths")
                .and_then(|v| v.as_array())
                .map(|a| normalize_rel_path_list(a))
                .unwrap_or_default();
            let context = map
                .get("context")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let files = map
                .get("files")
                .and_then(|v| v.as_array())
                .map(|a| normalize_rel_path_list(a))
                .unwrap_or_default();
            (write, paths, context, files)
        }
        _ => (false, Vec::new(), None, Vec::new()),
    };
    Some(SubagentTask {
        label,
        write,
        paths,
        context,
        files,
    })
}

/// Two writer scopes overlap when a path is equal or nested either way.
/// Sibling writers must not overlap: concurrent writes to one file tangle
/// attribution and undo beyond what epochs can separate.
fn scopes_overlap(a: &[String], b: &[String]) -> bool {
    a.iter().any(|x| {
        b.iter()
            .any(|y| x == y || x.starts_with(&format!("{y}/")) || y.starts_with(&format!("{x}/")))
    })
}

pub(crate) fn subagent_tasks_from_args(
    args: &serde_json::Value,
) -> Result<Vec<SubagentTask>, String> {
    let tasks: Vec<SubagentTask> = match args.get("tasks") {
        Some(serde_json::Value::Array(items)) => {
            items.iter().filter_map(subagent_task_spec).collect()
        }
        // a lone string is one task, not a malformed array
        Some(single) => subagent_task_spec(single).into_iter().collect(),
        None => Vec::new(),
    };
    if tasks.is_empty() {
        return Err(
            "subagent tasks are required: pass tasks (array of strings or objects with task|prompt)"
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
    web_allow_hosts: &[String],
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
                    "args": tools::call_args_for_journal(&call.args),
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
                let web_allow_hosts = web_allow_hosts.to_vec();
                let system = system.to_vec();
                let mcp = mcp.clone();
                let lsp = lsp.clone();
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
                        &web_allow_hosts,
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
/// ) and after a subagent returns (the child may have retired or
/// moved the held step). `ctx.current_step` stays the single live
/// copy; the TUI persists it via `StepCurrent` events.
pub(crate) fn adopt_in_progress_step(root: &Path, session_id: &str) -> Option<String> {
    plan::open_active_for_session(root, Some(session_id))
        .ok()
        .flatten()
        .and_then(|plan| {
            plan.steps
                .iter()
                .find(|step| step.status == plan::StepStatus::InProgress)
                .or_else(|| {
                    plan.steps
                        .iter()
                        .find(|step| step.status == plan::StepStatus::Reopened)
                })
                .map(|step| step.id.clone())
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
    web_allow_hosts: &[String],
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
                let task_label = task.label.clone();
                let mut one = call.clone();
                // re-parseable single: the recursive call re-derives the
                // same write scope from these flags
                one.args = serde_json::json!({
                    "tasks": [{
                        "task": task.label,
                        "write": task.write,
                        "paths": task.paths,
                        "context": task.context,
                        "files": task.files,
                    }],
                });
                let system = system.clone();
                let mcp = mcp.clone();
                let lsp = lsp.clone();
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
                        web_allow_hosts,
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
                        memory,
                        compaction,
                        plan_limits,
                        fallback_chain,
                        timeout,
                    )
                    .await;
                    (index, task_label, outcome)
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
    // NOTE: children always complete inside this tool call — the
    // event loop below is awaited before the outcome returns. There is no
    // fire-and-forget spawn, so `plan finish` can never race still-running
    // children of the same step; no extra gate is needed for that.
    // Inherit the spawning step, if any: the child stamps this
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
    // Keep all cacheable system parts byte-identical to the parent so every
    // child hits the parent's warm KV cache for `tools + stable system`, and
    // retain only `Mode:` + `<turn_environment>` in the volatile tail (dropping
    // parent-only one-shot notices like ANCHOR, stopped-turn, or undo notes),
    // then append the child's `<subagent_contract>` and `<parent_context>`.
    let mut child_system: Vec<SystemPart> = system
        .into_iter()
        .filter(|p| {
            p.cacheable
                || p.text.starts_with("Mode: ")
                || p.text.starts_with("<turn_environment>")
        })
        .collect();
    child_system.push(SystemPart::volatile(build_subagent_tail(
        root,
        parent_session,
        &task,
        child_read_only,
    )));
    let user_prompt = build_subagent_user_prompt(&task);
    let child = spawn_agent(AgentInput {
        provider: provider.clone(),
        model_id: model_id.to_string(),
        model_key: format!("sub-{id}"),
        effort,
        effort_support,
        max_tokens,
        system: child_system,
        messages: vec![Message::new(Role::User, user_prompt)],
        root: root.to_path_buf(),
        // #190: NOT `sub-{id}` — the numeric counter resets on restart
        // and would append to a previous run's journal file
        session_id: child_session.clone(),
        blocked_patterns: blocked_patterns.to_vec(),
        web_allow_hosts: web_allow_hosts.to_vec(),
        plan_mode,
        context_limit,
        enable_tools: true,
        // subagent baselines stay synchronous: short tasks, and progress
        // events would land on the parent's channel out of context
        background_baselines: false,
        read_only: child_read_only,
        previous_response_id: None,
        summary: None,
        mcp,
        lsp,
        compact_only: false, compact_focus: String::new(),
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
    let mut trail = SubagentTrail::default();
    let deadline = tokio::time::Instant::now() + timeout;
    // Esc polling: the parent cancel flag is the only stop signal visible
    // inside this wait — without it a stop request sits unobserved until
    // the timeout, and the TUI shows "cancelling…" forever.
    let mut cancel_poll = tokio::time::interval(std::time::Duration::from_millis(100));
    loop {
        if parent_cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return stop_child(
                &mut child,
                parent_tx,
                id,
                "cancelled by user",
                root,
                &child_session,
                &mut trail,
                &output,
            )
            .await;
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
                return stop_child(
                    &mut child,
                    parent_tx,
                    id,
                    &format!("timed out after {}s", timeout.as_secs()),
                    root,
                    &child_session,
                    &mut trail,
                    &output,
                )
                .await;
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
                trail.record_start(&name, &summary);
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
                trail.record_done(&name, ok);
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
            AgentEvent::Checkpoint { sha, label } => {
                let _ = parent_tx.send(AgentEvent::Checkpoint { sha, label }).await;
            }
            AgentEvent::Completed(result) => {
                trail.collect_journal_writes(root, &child_session);
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
                        let final_text = trail.attach_receipt(&output, true);
                        tools::Outcome::ok(final_text)
                    }
                    Err(error) => {
                        let final_err = trail.salvage_on_error(&error, &output);
                        tools::Outcome::err(final_err)
                    }
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
            _ => {}
        }
    }
}

/// Stop a child that will not stop itself: ask cooperatively first (a
/// `bash` child kills its own process tree on this), wait out a short
/// grace, then tear down hard. Always ends with `SubagentDone(ok:false)`
/// and the error outcome — the parent turn must never hang on a child.
/// Salvages any tool trail, modified files, and partial output accumulated
/// before the stop so the parent does not lose the child's investigation.
#[allow(clippy::too_many_arguments)]
async fn stop_child(
    child: &mut AgentHandle,
    parent_tx: &mpsc::Sender<AgentEvent>,
    id: u64,
    reason: &str,
    root: &Path,
    child_session: &str,
    trail: &mut SubagentTrail,
    partial_output: &str,
) -> tools::Outcome {
    child.request_tool_cancel();
    let grace = std::time::Duration::from_secs(SUBAGENT_CANCEL_GRACE_SECS);
    let finished = tokio::time::timeout(grace, child.rx.recv()).await;
    child.abort();
    trail.collect_journal_writes(root, child_session);
    let base_msg = if matches!(finished, Ok(Some(AgentEvent::Completed(_)))) {
        format!("subagent {reason} (finished during cancel, result discarded)")
    } else {
        format!("subagent {reason}")
    };
    let salvaged = trail.salvage_on_error(&base_msg, partial_output);
    let result = tools::Outcome::err(salvaged);
    let _ = parent_tx
        .send(AgentEvent::SubagentDone {
            id,
            ok: false,
            output: result.output.clone(),
        })
        .await;
    result
}

#[derive(Default, Debug)]
pub(crate) struct SubagentTrail {
    pub(crate) calls: Vec<String>,
    pub(crate) failed_calls: usize,
    pub(crate) modified_files: Vec<String>,
}

impl SubagentTrail {
    pub(crate) fn record_start(&mut self, name: &str, summary: &str) {
        let s = summary.trim();
        let entry = if s.is_empty() {
            name.to_string()
        } else {
            let clipped: String = s.chars().take(48).collect();
            let suffix = if s.chars().count() > 48 { "…" } else { "" };
            format!("{name}({clipped}{suffix})")
        };
        if self.calls.len() < 32 {
            self.calls.push(entry);
        }
    }

    pub(crate) fn record_done(&mut self, _name: &str, ok: bool) {
        if !ok {
            self.failed_calls += 1;
        }
    }

    pub(crate) fn collect_journal_writes(&mut self, root: &Path, child_session: &str) {
        let Ok(records) = crate::agent::journal::Journal::records_for(root, child_session) else {
            return;
        };
        for rec in records {
            if rec.kind == "file_diff"
                && let Some(path) = rec.fields.get("path").and_then(|v| v.as_str())
                && !path.is_empty()
                && !self.modified_files.iter().any(|p| p == path)
            {
                self.modified_files.push(path.to_string());
            }
        }
    }

    pub(crate) fn receipt_line(&self, ok: bool) -> Option<String> {
        if self.calls.is_empty() && self.modified_files.is_empty() {
            return None;
        }
        let status = if ok { "ok" } else { "stopped" };
        let calls_part = if self.calls.is_empty() {
            "0 tool calls".to_string()
        } else {
            let shown: Vec<&str> = self.calls.iter().take(12).map(String::as_str).collect();
            let extra = if self.calls.len() > 12 {
                format!(" +{} more", self.calls.len() - 12)
            } else {
                String::new()
            };
            let fail_note = if self.failed_calls > 0 {
                format!(" ({} failed)", self.failed_calls)
            } else {
                String::new()
            };
            format!(
                "{} tool call(s){fail_note}: {}{extra}",
                self.calls.len(),
                shown.join(", ")
            )
        };
        let mod_part = if self.modified_files.is_empty() {
            String::new()
        } else {
            format!(" | modified: {}", self.modified_files.join(", "))
        };
        Some(format!("[Subagent receipt: {status} | {calls_part}{mod_part}]"))
    }

    pub(crate) fn attach_receipt(&self, output: &str, ok: bool) -> String {
        let trimmed = output.trim();
        match self.receipt_line(ok) {
            Some(receipt) if trimmed.is_empty() => receipt,
            Some(receipt) => format!("{trimmed}\n\n{receipt}"),
            None => trimmed.to_string(),
        }
    }

    pub(crate) fn salvage_on_error(&self, err_msg: &str, partial_output: &str) -> String {
        let mut parts = vec![err_msg.to_string()];
        if let Some(receipt) = self.receipt_line(false) {
            parts.push(receipt);
        }
        let partial = partial_output.trim();
        if !partial.is_empty() {
            let capped: String = partial.chars().take(4_000).collect();
            parts.push(format!("Partial findings before stop:\n{capped}"));
        }
        parts.join("\n\n")
    }
}

pub(crate) fn build_subagent_user_prompt(task: &SubagentTask) -> String {
    let mut prompt = task.label.clone();
    let mut focus_files = task.files.clone();
    if !task.write {
        for p in &task.paths {
            if !focus_files.contains(p) {
                focus_files.push(p.clone());
            }
        }
    }
    if !focus_files.is_empty() {
        prompt.push_str(&format!("\n\nTarget files/paths: {}", focus_files.join(", ")));
    }
    if let Some(ref ctx) = task.context {
        prompt.push_str(&format!("\n\nAdditional context from parent:\n{ctx}"));
    }
    prompt
}

pub(crate) fn build_subagent_tail(
    root: &Path,
    parent_session: &str,
    task: &SubagentTask,
    child_read_only: bool,
) -> String {
    let mut block = String::new();
    if child_read_only {
        block.push_str(
            "<subagent_contract>\n\
             Role: READ-ONLY EXPLORER SUBAGENT\n\
             - You are running in an isolated child context to investigate and report back to the parent agent.\n\
             - Do NOT call mutating tools (`write`, `edit`, `multi_edit`, `patch`, `git_commit`, `git_stage`), `plan`, `memory_write`, `subagent`, or `ask_user`.\n\
             - Emit independent `grep`, `glob`, `outline`, and `read(offset, limit)` calls in parallel batches.\n\
             - Structure your final report concisely for the parent agent:\n\
               1. **Findings:** Verified facts with exact `path:line` references and symbol names.\n\
               2. **Key Signatures / Snippets:** Exact type/function signatures or critical lines so the parent does not need to re-read the same files.\n\
               3. **Risks / Edge Cases:** Callers, invariants, or edge cases discovered.\n\
             </subagent_contract>",
        );
    } else {
        let scope_list = if task.paths.is_empty() {
            "(none)".to_string()
        } else {
            task.paths.join(", ")
        };
        block.push_str(&format!(
            "<subagent_contract>\n\
             Role: SCOPED WRITER SUBAGENT\n\
             Allowed write scope: [{scope_list}]\n\
             - You are running in an isolated child context to implement changes strictly inside the allowed write scope above. Writes outside those paths are rejected by the host.\n\
             - Do NOT call `plan`, `memory_write`, `git_commit`, `subagent`, or `ask_user` — the parent agent owns plan state, git commits, memory, and user interaction.\n\
             - Read target files before overwriting or editing, and verify your changes (e.g. `cargo check` or targeted tests) before finishing when feasible.\n\
             - Structure your final report concisely for the parent agent:\n\
               1. **Changes Made:** Exact files and symbols modified (`path:line`) and what changed.\n\
               2. **Verification:** Checks run and their outcome (or why verification was skipped).\n\
               3. **Interface / Follow-up Impact:** Any cross-file impacts outside your write scope that the parent must handle.\n\
             </subagent_contract>"
        ));
    }

    if let Some(parent_ctx) = build_parent_context_brief(root, parent_session) {
        block.push_str("\n\n");
        block.push_str(&parent_ctx);
    }
    block
}

pub(crate) fn build_parent_context_brief(root: &Path, parent_session: &str) -> Option<String> {
    if parent_session.trim().is_empty() {
        return None;
    }
    let mut lines: Vec<String> = Vec::new();

    if let Ok(Some(active_plan)) = plan::open_active_for_session(root, Some(parent_session)) {
        lines.push(format!("Plan goal: {}", active_plan.goal.text));
        if let Some(in_prog) = active_plan
            .steps
            .iter()
            .find(|s| s.status == plan::StepStatus::InProgress)
        {
            lines.push(format!("Active step [{}]: {}", in_prog.id, in_prog.title));
        }
        if !active_plan.constraints.is_empty() {
            lines.push(format!(
                "Plan constraints: {}",
                active_plan.constraints.join("; ")
            ));
        }
    }

    if let Ok(records) = crate::agent::journal::Journal::records_for(root, parent_session) {
        let mut modified: Vec<String> = Vec::new();
        let mut inspected: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();

        for rec in records.iter().rev() {
            match rec.kind.as_str() {
                "file_diff" => {
                    if modified.len() < 8
                        && let Some(p) = rec.fields.get("path").and_then(|v| v.as_str())
                        && !p.is_empty()
                        && !modified.iter().any(|x| x == p)
                    {
                        modified.push(p.to_string());
                    }
                }
                "tool_call" => {
                    let tool = rec.fields.get("tool").and_then(|v| v.as_str()).unwrap_or("");
                    if matches!(tool, "read" | "outline")
                        && inspected.len() < 10
                        && let Some(p) = rec.fields.get("path").and_then(|v| v.as_str())
                        && !p.is_empty()
                        && !inspected.iter().any(|x| x == p)
                    {
                        inspected.push(p.to_string());
                    }
                }
                "note" => {
                    if notes.len() < 5 {
                        let (kind, text) = match (
                            rec.fields.get("note").and_then(|v| v.as_str()),
                            rec.fields.get("text").and_then(|v| v.as_str()),
                        ) {
                            (Some(k), Some(t)) if !t.is_empty() => (k, t),
                            (Some(t), None) if !t.is_empty() => ("note", t),
                            _ => continue,
                        };
                        let entry = format!("[{kind}] {text}");
                        if !notes.contains(&entry) {
                            notes.push(entry);
                        }
                    }
                }
                _ => {}
            }
        }

        if !modified.is_empty() {
            lines.push(format!(
                "Parent modified files: {}",
                modified.join(", ")
            ));
        }
        if !inspected.is_empty() {
            lines.push(format!(
                "Parent recently read files: {}",
                inspected.join(", ")
            ));
        }
        if !notes.is_empty() {
            lines.push(format!("Parent session notes: {}", notes.join(" | ")));
        }
    }

    if lines.is_empty() {
        None
    } else {
        Some(format!(
            "<parent_context>\n{}\n</parent_context>",
            lines.join("\n")
        ))
    }
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
